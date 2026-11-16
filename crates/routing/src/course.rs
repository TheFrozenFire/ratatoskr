//! An ordered sequence of slopes.
//!
//! The output of one leg is the input of the next. Preparing the legs once
//! makes a push through the sequence a prefix lookup on each leg.

use num_bigint::{BigInt, Sign};
use num_traits::Zero;

use crate::book::{Cover, Run};
use crate::pay::u128_of;
use crate::SlopeBook;

/// Ordered slopes of one route, prepared for a push from the source to the sink.
pub struct Course {
    legs: Vec<SlopeBook>,
}

impl Course {
    /// `None` when `legs` is empty.
    pub fn new(legs: Vec<SlopeBook>) -> Option<Self> {
        if legs.is_empty() {
            None
        } else {
            Some(Self { legs })
        }
    }

    /// Sink output of `input` when every leg still holds a zero tender.
    pub fn quote(&self, input: &BigInt) -> BigInt {
        let mut flow = input.clone();
        for leg in &self.legs {
            if flow.sign() != Sign::Plus {
                return BigInt::from(0);
            }
            flow = leg.quote(&flow);
        }
        flow
    }

    /// Largest source at or below `budget` that downstream legs can still absorb.
    ///
    /// `None` when a tender is negative or a leg is not native.
    pub fn useful(&self, tenders: &[&BigInt], budget: &BigInt) -> Option<BigInt> {
        if tenders.len() != self.legs.len() || budget.sign() == Sign::Minus {
            return None;
        }
        let mut wanted: Option<BigInt> = None;
        for (leg, tender) in self.legs.iter().zip(tenders).rev() {
            let room = leg.remaining(tender)?;
            let useful = match &wanted {
                None => room,
                Some(want) => match leg.least_input(tender, want) {
                    Cover::Take(take) => BigInt::from(take),
                    Cover::Short => room,
                    Cover::Wide => return None,
                },
            };
            wanted = Some(useful);
        }
        let useful = wanted?;
        if &useful < budget {
            Some(useful)
        } else {
            Some(budget.clone())
        }
    }

    /// Source at or below `limit` that stays on an integer rate on every leg.
    ///
    /// `None` when the opening is not an integer rate, so the chord span still
    /// applies. `Some(None)` when the amount does not fit in machine words.
    pub fn linear_take(&self, tenders: &[&BigInt], limit: &BigInt) -> Option<Option<BigInt>> {
        if tenders.len() != self.legs.len() || limit.sign() != Sign::Plus {
            return Some(None);
        }
        let Some(mut source_cap) = u128_of(limit) else {
            return Some(None);
        };
        let mut out_per_source = 1u128;
        for (index, (leg, tender)) in self.legs.iter().zip(tenders).enumerate() {
            let (rate, allow) = match leg.integer_run(tender) {
                Run::Break => return None,
                Run::Wide => return Some(None),
                Run::Rate { rate, allow } => (rate, allow),
            };
            if allow == 0 || out_per_source == 0 {
                return None;
            }
            let max_source = allow / out_per_source;
            if source_cap > max_source {
                source_cap = max_source;
            }
            if index + 1 == self.legs.len() {
                break;
            }
            let Some(next) = out_per_source.checked_mul(rate) else {
                return Some(None);
            };
            out_per_source = next;
        }
        if source_cap == 0 {
            None
        } else {
            Some(Some(BigInt::from(source_cap)))
        }
    }

    /// Largest source at or below `limit` that keeps every leg on its open chord.
    ///
    /// `None` when no positive source does, or when the amount is not native.
    /// A zero result means the open chords do not admit a whole step.
    pub fn open_span(&self, tenders: &[&BigInt], limit: &BigInt) -> Option<BigInt> {
        if tenders.len() != self.legs.len() || limit.sign() != Sign::Plus {
            return None;
        }
        let mut cap = u128_of(limit);
        let mut bounded = false;
        for index in (0..self.legs.len()).rev() {
            match enter_cap(&self.legs[index], tenders[index])? {
                None => continue,
                Some(0) => return None,
                Some(enter_cap) => match invert(self, tenders, index, enter_cap)? {
                    None => continue,
                    Some(allowed) => {
                        cap = Some(cap.map(|held| held.min(allowed)).unwrap_or(allowed));
                        bounded = true;
                    }
                },
            }
        }
        let take = if bounded { cap? } else { u128_of(limit)? };
        if take == 0 {
            return None;
        }
        let take = BigInt::from(take);
        if &take > limit {
            return None;
        }
        if !self.stays(tenders, &take) {
            return None;
        }
        if &take < limit {
            let next = &take + 1;
            if self.stays(tenders, &next) {
                return None;
            }
        }
        Some(take)
    }

    /// Largest source above one unit that ends on the chord one unit reaches.
    pub fn landing(&self, tenders: &[&BigInt], limit: &BigInt) -> Option<BigInt> {
        if tenders.len() != self.legs.len() || limit <= &BigInt::from(1) {
            return None;
        }
        let (takes, _) = self.push(tenders, &BigInt::from(1))?;
        let mut cap = u128_of(limit)?;
        let mut bound = false;
        for index in 0..self.legs.len() {
            let Some(room) = self.legs[index].landing_room(tenders[index], &takes[index]) else {
                continue;
            };
            if room == 0 {
                return None;
            }
            bound = true;
            if let Some(allowed) = invert(self, tenders, index, room)? {
                cap = cap.min(allowed);
            }
        }
        if !bound || cap <= 1 {
            None
        } else {
            Some(BigInt::from(cap))
        }
    }

    /// Per-leg takes and the sink output of pushing `source` through `tenders`.
    ///
    /// The source is not capped. A leg may cross more than one chord.
    pub fn push(&self, tenders: &[&BigInt], source: &BigInt) -> Option<(Vec<BigInt>, BigInt)> {
        self.walk(tenders, source, false)
    }

    /// Per-leg takes and the sink output of pushing `source` through `tenders`.
    pub fn drive(&self, tenders: &[&BigInt], source: &BigInt) -> Option<(Vec<BigInt>, BigInt)> {
        self.walk(tenders, source, true)
    }

    fn walk(
        &self,
        tenders: &[&BigInt],
        source: &BigInt,
        open_chord: bool,
    ) -> Option<(Vec<BigInt>, BigInt)> {
        if tenders.len() != self.legs.len() || source.sign() != Sign::Plus {
            return None;
        }
        let mut enter = u128_of(source)?;
        let source_u = enter;
        let mut takes = Vec::with_capacity(self.legs.len());
        for (index, (leg, tender)) in self.legs.iter().zip(tenders).enumerate() {
            let room = leg.remaining_u128(tender)?;
            let take = enter.min(room);
            if index == 0 && take != source_u {
                return None;
            }
            if open_chord && take > 0 {
                match leg.paying_width(tender) {
                    Some(Some(rem_w)) if take <= rem_w => {}
                    _ => return None,
                }
            }
            takes.push(BigInt::from(take));
            enter = leg.delta_u128(tender, &takes[index])?;
        }
        let gain = BigInt::from(enter);
        if gain.is_zero() {
            None
        } else {
            Some((takes, gain))
        }
    }

    /// Takes and sink output of `source` when every leg stays on one rate.
    ///
    /// The source is not capped. `None` when a leg would cross onto another rate.
    pub fn same_rate(&self, tenders: &[&BigInt], source: &BigInt) -> Option<(Vec<BigInt>, BigInt)> {
        if tenders.len() != self.legs.len() || source.sign() != Sign::Plus {
            return None;
        }
        let mut enter = u128_of(source)?;
        let source_u = enter;
        let mut takes = Vec::with_capacity(self.legs.len());
        for (index, (leg, tender)) in self.legs.iter().zip(tenders).enumerate() {
            let room = leg.remaining_u128(tender)?;
            let take = enter.min(room);
            if index == 0 && take != source_u {
                return None;
            }
            let take_int = BigInt::from(take);
            if !leg.one_rate(tender, &take_int)? {
                return None;
            }
            takes.push(take_int);
            enter = leg.delta_u128(tender, &takes[index])?;
        }
        let gain = BigInt::from(enter);
        if gain.is_zero() {
            None
        } else {
            Some((takes, gain))
        }
    }

    /// Least per-leg takes whose sink output is at least `need`.
    ///
    /// The source take is at most `budget`. `None` when the legs cannot fund
    /// `need` inside that budget.
    pub fn least_push(
        &self,
        tenders: &[&BigInt],
        need: &BigInt,
        budget: &BigInt,
    ) -> Option<(Vec<BigInt>, BigInt)> {
        if tenders.len() != self.legs.len()
            || need.sign() != Sign::Plus
            || budget.sign() == Sign::Minus
        {
            return None;
        }
        let mut required = need.clone();
        let mut takes = vec![BigInt::from(0); self.legs.len()];
        for index in (0..self.legs.len()).rev() {
            let take = match self.legs[index].least_input(tenders[index], &required) {
                Cover::Take(take) => BigInt::from(take),
                Cover::Short | Cover::Wide => return None,
            };
            if take.is_zero() {
                return None;
            }
            required = take.clone();
            takes[index] = take;
        }
        if &takes[0] > budget {
            return None;
        }
        let delivered = self
            .legs
            .last()?
            .delta_u128(tenders.last()?, takes.last()?)?;
        let delivered = BigInt::from(delivered);
        if &delivered < need {
            return None;
        }
        Some((takes, delivered))
    }

    /// Every leg's take stays on the chord open at its tender, and the source is not capped.
    pub fn stays(&self, tenders: &[&BigInt], source: &BigInt) -> bool {
        self.drive(tenders, source).is_some()
    }
}

/// `Some(room)` binds. `None` inside `Some` does not bind. Outer `None` is not native.
fn enter_cap(leg: &SlopeBook, tender: &BigInt) -> Option<Option<u128>> {
    let room = leg.remaining_u128(tender)?;
    if room == 0 {
        return Some(None);
    }
    match leg.paying_width(tender)? {
        None => Some(Some(0)),
        Some(open) if room <= open => Some(None),
        Some(open) => Some(Some(open)),
    }
}

/// `Some(Some(source))` binds. `Some(None)` does not. `None` is not native.
fn invert(
    course: &Course,
    tenders: &[&BigInt],
    index: usize,
    enter_cap: u128,
) -> Option<Option<u128>> {
    let mut max_enter = enter_cap;
    for cursor in (0..=index).rev() {
        if cursor == 0 {
            return Some(Some(max_enter));
        }
        let prev = &course.legs[cursor - 1];
        let room = prev.remaining_u128(tenders[cursor - 1])?;
        if max_enter == u128::MAX {
            return None;
        }
        let take = match prev.least_input(tenders[cursor - 1], &BigInt::from(max_enter + 1)) {
            Cover::Wide => return None,
            Cover::Short => return Some(None),
            Cover::Take(0) => 0,
            Cover::Take(take) => take - 1,
        };
        if take >= room {
            return Some(None);
        }
        max_enter = take;
    }
    Some(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{quote, Action, SlopeSegment};

    fn segment(width: i64, output: i64) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: Action {
                kind: 1,
                pool: [1; 32],
            },
        }
    }

    fn book(segments: &[SlopeSegment]) -> SlopeBook {
        SlopeBook::prepare(
            segments
                .iter()
                .map(|segment| (&segment.width, &segment.output)),
        )
        .expect("prefix")
    }

    #[test]
    fn the_course_quote_is_the_legs_in_order() {
        let first = vec![segment(4, 10), segment(4, 6)];
        let second = vec![segment(5, 5), segment(8, 4)];
        let course = Course::new(vec![book(&first), book(&second)]).expect("course");
        for input in 0..=12 {
            let input = BigInt::from(input);
            let mid = quote(&first, &input);
            assert_eq!(course.quote(&input), quote(&second, &mid));
        }
    }

    #[test]
    fn the_open_span_stops_at_the_next_chord() {
        let course = Course::new(vec![
            book(&[segment(10, 10)]),
            book(&[segment(4, 8), segment(10, 10)]),
        ])
        .expect("course");
        let zero = BigInt::from(0);
        let tenders = [&zero, &zero];
        let limit = BigInt::from(10);
        let span = course.open_span(&tenders, &limit).expect("span");
        assert_eq!(span, BigInt::from(4));
        let (takes, gain) = course.drive(&tenders, &span).expect("drive");
        assert_eq!(takes, vec![BigInt::from(4), BigInt::from(4)]);
        assert_eq!(gain, BigInt::from(8));
    }

    #[test]
    fn the_least_push_is_the_first_source_that_funds_the_sink() {
        let first = vec![segment(4, 10), segment(4, 6)];
        let second = vec![segment(5, 5), segment(8, 4)];
        let course = Course::new(vec![book(&first), book(&second)]).expect("course");
        let zero = BigInt::from(0);
        let tenders = [&zero, &zero];
        let budget = BigInt::from(8);
        for need in 1..=9 {
            let need = BigInt::from(need);
            let (takes, gain) = course.least_push(&tenders, &need, &budget).expect("funded");
            assert!(gain >= need);
            let legs = [&first, &second];
            let mut required = need.clone();
            for (segments, take) in legs.into_iter().zip(&takes).rev() {
                assert!(quote(segments, take) >= required);
                if take > &BigInt::from(1) {
                    assert!(quote(segments, &(take - 1)) < required);
                }
                required = take.clone();
            }
            assert_eq!(gain, quote(&second, &takes[1]));
        }
    }

    #[test]
    fn the_same_rate_crosses_equal_chords() {
        let course = Course::new(vec![
            book(&[segment(4, 2), segment(6, 3)]),
            book(&[segment(10, 10)]),
        ])
        .expect("course");
        let zero = BigInt::from(0);
        let tenders = [&zero, &zero];
        let (takes, gain) = course
            .same_rate(&tenders, &BigInt::from(10))
            .expect("one rate");
        assert_eq!(takes, vec![BigInt::from(10), BigInt::from(5)]);
        assert_eq!(gain, BigInt::from(5));
        let steeper = Course::new(vec![
            book(&[segment(4, 2), segment(6, 6)]),
            book(&[segment(10, 10)]),
        ])
        .expect("course");
        assert!(steeper.same_rate(&tenders, &BigInt::from(5)).is_none());
    }
}
