//! Key state for the status screens. The draw loop renders this. It does not
//! read the catalog.

use alloy_primitives::{Address, B256};
use serde_json::Value;

use super::ingest::{ingest_from, Ingest, Local};

pub(crate) const VIEWS: [&str; 9] = [
    "ingestion",
    "survey",
    "peers",
    "work",
    "pools",
    "conduits",
    "tokens",
    "feeds",
    "spots",
];

pub(crate) const PAGE: usize = 32;

const SURVEY: usize = 1;
const WORK: usize = 3;
const POOLS: usize = 4;
const CONDUITS: usize = 5;
const TOKENS: usize = 6;

const WORK_STAGES: [Option<u8>; 6] = [None, Some(1), Some(2), Some(3), Some(4), Some(6)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    Nav,
    Body,
    Detail,
    Filter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    None,
    Prepare,
    Reload,
    Measure(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowKey {
    Pool {
        kind: u8,
        id: B256,
    },
    Conduit {
        token_in: Address,
        token_out: Address,
    },
    Token(Address),
    Feed(Address),
    Spot {
        token: Address,
        numeraire: Address,
    },
    Peer(usize),
    Work(usize),
    ConduitWork(usize),
}

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub cells: Vec<String>,
    pub key: RowKey,
    pub detail: String,
    pub resume: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SurveyRow {
    pub name: String,
    pub class: String,
    pub retained: String,
    pub value_in: String,
    pub value_out: String,
    pub count: String,
    pub gas: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SurveyCtl {
    pub items: Vec<SurveyRow>,
    pub cursor: usize,
    pub detail: bool,
    pub scroll: u16,
    pub busy: bool,
    pub follow: bool,
    pub order: Order,
    pub note: String,
}

impl Default for SurveyCtl {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            cursor: 0,
            detail: false,
            scroll: 0,
            busy: false,
            follow: false,
            order: Order::None,
            note: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nudge {
    Stay,
    Next,
    Prev,
}

pub(crate) struct App {
    pub nav: usize,
    pub view: usize,
    pub focus: Focus,
    pub cursor: usize,
    pub scroll: u16,
    pub rows: Vec<Row>,
    pub page_full: bool,
    pub nudge: Nudge,
    pub reload: u64,
    pub filter: String,
    pub applied: String,
    pub ingest: Ingest,
    pub detail: String,
    pub detail_pending: bool,
    pub quit: bool,
    pub control: Option<&'static str>,
    pub survey: SurveyCtl,
    pub work_conduit: bool,
    pub work_stage: Option<u8>,
    pub work_step: usize,
    pub pool_kind: Option<u8>,
    pub pool_step: usize,
    pub live_peers: Vec<String>,
    pub peer_down: bool,
    pub sightings: Option<u64>,
    pub notice: String,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            nav: 0,
            view: 0,
            focus: Focus::Nav,
            cursor: 0,
            scroll: 0,
            rows: Vec::new(),
            page_full: false,
            nudge: Nudge::Stay,
            reload: 0,
            filter: String::new(),
            applied: String::new(),
            ingest: Ingest::default(),
            detail: String::new(),
            detail_pending: false,
            quit: false,
            control: None,
            survey: SurveyCtl::default(),
            work_conduit: false,
            work_stage: None,
            work_step: 0,
            pool_kind: None,
            pool_step: 0,
            live_peers: Vec::new(),
            peer_down: false,
            sightings: None,
            notice: String::new(),
        }
    }

    pub(crate) fn press(&mut self, key: Key) {
        if self.quit {
            return;
        }
        match key {
            Key::Char('q') => self.quit = true,
            Key::Char(ch @ '1'..='4') if self.focus != Focus::Filter => {
                self.jump(ch as usize - '1' as usize);
            }
            Key::Esc => self.esc(),
            Key::Enter => self.enter(),
            Key::Up | Key::Char('k') => self.shift(-1),
            Key::Down | Key::Char('j') => self.shift(1),
            Key::Char('h') if self.focus != Focus::Filter => self.focus = Focus::Nav,
            Key::Char('p') if self.focus != Focus::Filter && self.view == 0 => {
                if let Some(paused) = self.ingest.paused {
                    self.control = Some(super::ingest::control_path(paused));
                }
            }
            Key::Char('s') if self.focus != Focus::Filter && self.view == WORK => self.cycle_work(),
            Key::Char('s') if self.focus != Focus::Filter && self.view == POOLS => {
                self.cycle_pools();
            }
            Key::Char('r') if self.focus != Focus::Filter && self.view == SURVEY => {
                self.measure_one();
            }
            Key::Char('R') if self.focus != Focus::Filter && self.view == SURVEY => {
                self.measure_all();
            }
            Key::Char('/')
                if self.focus != Focus::Filter && matches!(self.view, CONDUITS | TOKENS) =>
            {
                self.focus = Focus::Filter;
            }
            Key::Backspace if self.focus == Focus::Filter => {
                self.filter.pop();
            }
            Key::Char(ch) if self.focus == Focus::Filter && !ch.is_control() => {
                self.filter.push(ch);
            }
            _ => {}
        }
    }

    pub(crate) fn take_order(&mut self) -> Order {
        if self.survey.busy {
            return Order::None;
        }
        let order = std::mem::replace(&mut self.survey.order, Order::None);
        if order != Order::None {
            self.survey.busy = true;
        }
        order
    }

    pub(crate) fn take_control(&mut self) -> Option<&'static str> {
        self.control.take()
    }

    pub(crate) fn survey_prepared(&mut self, items: Vec<SurveyRow>) {
        self.survey.busy = false;
        self.survey.items = items;
        self.survey.cursor = self
            .survey
            .cursor
            .min(self.survey.items.len().saturating_sub(1));
        self.survey.note = format!("{} scenarios", self.survey.items.len());
        if self.survey.follow {
            self.arm_measure(0);
        }
    }

    pub(crate) fn survey_failed(&mut self, err: String) {
        self.survey.busy = false;
        self.survey.follow = false;
        self.survey.note = err;
    }

    pub(crate) fn survey_measured(&mut self, index: usize, text: String) {
        self.survey.busy = false;
        if let Some(row) = self.survey.items.get_mut(index) {
            row.retained = field(&text, "retained");
            row.value_in = field(&text, "value_in");
            row.value_out = field(&text, "value_out");
            row.count = field(&text, "count");
            row.gas = field(&text, "gas");
            row.text = text;
        }
        if matches!(self.survey.order, Order::Reload) {
            return;
        }
        if self.survey.follow {
            let next = index.saturating_add(1);
            if next < self.survey.items.len() {
                self.arm_measure(next);
            } else {
                self.survey.follow = false;
                self.survey.note = "survey finished".into();
            }
        } else {
            self.survey.note = format!("{} scenarios", self.survey.items.len());
        }
    }

    pub(crate) fn apply_status(&mut self, snap: Option<&Value>, local: &Local, rates: &[f64]) {
        self.ingest = ingest_from(snap, local, rates);
    }

    pub(crate) fn ingestion_text(&self) -> String {
        let mut lines = self.ingest.pipelines.clone();
        if !self.ingest.notice.is_empty() {
            lines.push(self.ingest.notice.clone());
        }
        lines.push(self.ingest.catalog.clone());
        for stage in &self.ingest.stages {
            lines.push(format!(
                "{} queued {} batches {} jobs {} last {} rate {}",
                stage.name, stage.queued, stage.batches, stage.jobs, stage.last, stage.rate
            ));
        }
        lines.join("\n")
    }

    fn jump(&mut self, index: usize) {
        self.nav = index;
        self.open_view(index);
    }

    fn open_view(&mut self, index: usize) {
        let changed = self.view != index;
        self.view = index;
        self.focus = Focus::Body;
        if changed {
            self.cursor = 0;
            self.scroll = 0;
            self.detail.clear();
            self.detail_pending = false;
            self.reload = self.reload.wrapping_add(1);
        }
        if index == SURVEY
            && self.survey.items.is_empty()
            && self.survey.order == Order::None
            && !self.survey.busy
        {
            self.survey.order = Order::Prepare;
            self.survey.note = "preparing".into();
        }
    }

    fn enter(&mut self) {
        match self.focus {
            Focus::Nav => self.open_view(self.nav),
            Focus::Filter => {
                self.applied = self.filter.clone();
                self.focus = Focus::Body;
                self.cursor = 0;
                self.reload = self.reload.wrapping_add(1);
            }
            Focus::Body if self.view == SURVEY => {
                if self.survey.items.is_empty() {
                    return;
                }
                self.survey.detail = true;
                self.survey.scroll = 0;
                self.focus = Focus::Detail;
            }
            Focus::Body => {
                if self.rows.is_empty() {
                    return;
                }
                self.focus = Focus::Detail;
                self.scroll = 0;
                self.detail_pending = true;
            }
            Focus::Detail => {}
        }
    }

    fn esc(&mut self) {
        if self.focus == Focus::Filter {
            self.filter = self.applied.clone();
            self.focus = Focus::Body;
            return;
        }
        if self.focus == Focus::Detail {
            self.focus = Focus::Body;
            self.survey.detail = false;
            self.scroll = 0;
            self.survey.scroll = 0;
            return;
        }
        if self.view == SURVEY
            && (self.survey.follow
                || matches!(self.survey.order, Order::Measure(_) | Order::Reload))
        {
            self.survey.follow = false;
            if !self.survey.busy {
                self.survey.order = Order::None;
            }
            self.survey.note = "stopped".into();
            return;
        }
        if self.focus == Focus::Body {
            self.focus = Focus::Nav;
        }
    }

    fn shift(&mut self, delta: isize) {
        match self.focus {
            Focus::Nav => {
                let next = self.nav as isize + delta;
                if (0..VIEWS.len() as isize).contains(&next) {
                    self.nav = next as usize;
                }
            }
            Focus::Body if self.view == SURVEY => {
                self.survey.cursor = move_index(self.survey.cursor, self.survey.items.len(), delta);
            }
            Focus::Body => {
                if delta > 0 && self.cursor + 1 >= self.rows.len() && self.page_full {
                    self.nudge = Nudge::Next;
                    return;
                }
                if delta < 0 && self.cursor == 0 {
                    self.nudge = Nudge::Prev;
                    return;
                }
                self.cursor = move_index(self.cursor, self.rows.len(), delta);
            }
            Focus::Detail if self.view == SURVEY && self.survey.detail => {
                self.survey.scroll = slide(self.survey.scroll, delta);
            }
            Focus::Detail => {
                self.scroll = slide(self.scroll, delta);
            }
            Focus::Filter => {}
        }
    }

    fn measure_one(&mut self) {
        if self.survey.items.is_empty() {
            self.survey.order = Order::Prepare;
            self.survey.note = "preparing".into();
            return;
        }
        self.survey.follow = false;
        self.arm_measure(self.survey.cursor);
    }

    fn measure_all(&mut self) {
        self.survey.follow = true;
        self.survey.detail = false;
        if self.focus == Focus::Detail {
            self.focus = Focus::Body;
        }
        if self.survey.busy {
            self.survey.order = Order::Reload;
            return;
        }
        self.survey.order = Order::Reload;
        self.survey.note = "refreshing survey".into();
    }

    fn arm_measure(&mut self, index: usize) {
        if self.survey.busy {
            self.survey.order = Order::Measure(index);
            return;
        }
        self.survey.order = Order::Measure(index);
        if let Some(row) = self.survey.items.get(index) {
            self.survey.note = format!("measuring {}", row.name);
        }
    }

    fn cycle_work(&mut self) {
        self.work_step = (self.work_step + 1) % (WORK_STAGES.len() + 1);
        if self.work_step == WORK_STAGES.len() {
            self.work_conduit = true;
            self.work_stage = None;
        } else {
            self.work_conduit = false;
            self.work_stage = WORK_STAGES[self.work_step];
        }
        self.cursor = 0;
        self.reload = self.reload.wrapping_add(1);
    }

    fn cycle_pools(&mut self) {
        self.pool_step = (self.pool_step + 1) % 15;
        self.pool_kind = if self.pool_step == 0 {
            None
        } else {
            Some(self.pool_step as u8)
        };
        self.cursor = 0;
        self.reload = self.reload.wrapping_add(1);
    }
}

fn move_index(cursor: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = cursor as isize + delta;
    if next < 0 {
        0
    } else if next as usize >= len {
        len - 1
    } else {
        next as usize
    }
}

fn slide(scroll: u16, delta: isize) -> u16 {
    if delta < 0 {
        scroll.saturating_sub(delta.unsigned_abs() as u16)
    } else {
        scroll.saturating_add(delta as u16)
    }
}

fn field(text: &str, label: &str) -> String {
    let prefix = format!("{label} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Value {
        serde_json::json!({
            "identities": 10,
            "block": 8,
            "uptime_ms": 1000,
            "paused": false,
            "board": {
                "headers": {"live": true, "cursor": 5, "head": 9, "head_known": true},
                "corpus": {"live": true, "bytes": 2048, "holes": 1},
                "tokens": {
                    "records": 4,
                    "records_known": true,
                    "missing": 2,
                    "missing_known": true,
                    "capped": false,
                    "reading": true
                },
                "feeds": {
                    "live": true,
                    "feeds": 3,
                    "configured": 3,
                    "answers": 8,
                    "jobs": 1,
                    "follow": 20,
                    "follow_known": true,
                    "lookback": true
                },
                "peers": {"live": false},
                "spots": {"live": true, "spots": 12}
            }
        })
    }

    fn two_rows() -> Vec<SurveyRow> {
        vec![
            SurveyRow {
                name: "weth".into(),
                class: "ordinary".into(),
                ..SurveyRow::default()
            },
            SurveyRow {
                name: "via".into(),
                class: "via".into(),
                ..SurveyRow::default()
            },
        ]
    }

    #[test]
    fn the_status_names_the_token_lens_and_the_feeds() {
        let mut app = App::new();
        app.apply_status(Some(&snap()), &Local::default(), &[]);
        let text = app.ingestion_text();
        assert!(text.contains("header sync"), "{text}");
        assert!(text.contains("cursor 5"), "{text}");
        assert!(text.contains("behind 4"), "{text}");
        assert!(text.contains("discovery"), "{text}");
        assert!(text.contains("identities 10"), "{text}");
        assert!(text.contains("token lens"), "{text}");
        assert!(text.contains("4 stored"), "{text}");
        assert!(text.contains("2 missing"), "{text}");
        assert!(text.contains("reading"), "{text}");
        assert!(text.contains("oracles"), "{text}");
        assert!(text.contains("8 answers"), "{text}");
        assert!(text.contains("spots"), "{text}");
        assert!(text.contains("12 spots"), "{text}");
        assert!(text.contains("following"), "{text}");
        assert!(text.contains("2.0 KB"), "{text}");
        assert!(text.contains("peer service"), "{text}");
        assert!(text.contains("not running"), "{text}");
    }

    #[test]
    fn exporter_down_shows_the_header_and_blanks_the_rates() {
        let local = Local {
            header_number: Some(42),
            header_timestamp: Some(9),
            cursors: vec![("list:test".into(), 40)],
            queued: [Some(3), None, None, None, None],
            ..Local::default()
        };
        let mut app = App::new();
        app.apply_status(None, &local, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        let text = app.ingestion_text();
        assert!(text.contains("exporter is down"), "{text}");
        assert!(text.contains("42"), "{text}");
        assert!(text.contains("list:test"), "{text}");
        assert!(text.contains("40"), "{text}");
        assert!(app.ingest.block.is_empty());
        assert!(app.ingest.stages.iter().all(|stage| stage.rate.is_empty()));
    }

    #[test]
    fn j_and_k_move_the_view_list_and_numbers_jump() {
        let mut app = App::new();
        app.press(Key::Char('j'));
        assert_eq!(app.nav, 1);
        assert_eq!(app.focus, Focus::Nav);
        app.press(Key::Char('2'));
        assert_eq!(app.view, 1);
        assert_eq!(app.focus, Focus::Body);
        assert_eq!(app.take_order(), Order::Prepare);
        app.press(Key::Char('1'));
        assert_eq!(app.view, 0);
        app.press(Key::Esc);
        assert_eq!(app.focus, Focus::Nav);
        assert!(!app.quit);
    }

    #[test]
    fn r_fills_one_survey_row_and_enter_shows_the_text() {
        let mut app = App::new();
        app.press(Key::Char('2'));
        let _ = app.take_order();
        app.survey_prepared(two_rows());
        app.press(Key::Char('r'));
        assert_eq!(app.take_order(), Order::Measure(0));
        let text =
            "== weth\ncount 4\ngas 9\nvalue_in 1.000000\nvalue_out 2.000000\nretained 0.500000\n";
        app.survey_measured(0, text.into());
        assert_eq!(app.survey.items[0].count, "4");
        assert_eq!(app.survey.items[0].gas, "9");
        assert_eq!(app.survey.items[0].retained, "0.500000");
        assert_eq!(app.survey.items[0].value_in, "1.000000");
        assert_eq!(app.survey.items[0].value_out, "2.000000");
        assert!(app.survey.items[1].text.is_empty());
        app.press(Key::Enter);
        assert!(app.survey.detail);
        assert!(app.survey.items[0].text.contains("count 4"));
    }

    #[test]
    fn esc_during_a_whole_survey_keeps_finished_rows() {
        let mut app = App::new();
        app.press(Key::Char('2'));
        let _ = app.take_order();
        app.survey_prepared(two_rows());
        app.press(Key::Char('R'));
        assert_eq!(app.take_order(), Order::Reload);
        app.survey_prepared(two_rows());
        assert_eq!(app.take_order(), Order::Measure(0));
        app.survey_measured(
            0,
            "count 1\ngas 2\nvalue_in 3\nvalue_out 4\nretained 5\n".into(),
        );
        app.press(Key::Esc);
        assert_eq!(app.take_order(), Order::None);
        assert!(!app.survey.follow);
        assert!(app.survey.items[0].text.contains("count 1"));
        assert!(app.survey.items[1].text.is_empty());
        assert!(!app.quit);
    }

    #[test]
    fn esc_while_one_scenario_is_running_does_not_start_the_next() {
        let mut app = App::new();
        app.press(Key::Char('2'));
        let _ = app.take_order();
        app.survey_prepared(two_rows());
        app.press(Key::Char('R'));
        let _ = app.take_order();
        app.survey_prepared(two_rows());
        assert_eq!(app.take_order(), Order::Measure(0));
        app.press(Key::Esc);
        app.survey_measured(0, "count 1\n".into());
        assert_eq!(app.take_order(), Order::None);
        assert!(app.survey.items[0].text.contains("count 1"));
    }

    #[test]
    fn q_quits_and_esc_does_not() {
        let mut app = App::new();
        app.press(Key::Esc);
        assert!(!app.quit);
        app.press(Key::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn p_pauses_until_the_snapshot_says_paused() {
        let mut app = App::new();
        app.apply_status(Some(&snap()), &Local::default(), &[]);
        app.press(Key::Char('p'));
        assert_eq!(app.take_control(), Some("/pause"));
        app.ingest.paused = Some(true);
        app.press(Key::Char('p'));
        assert_eq!(app.take_control(), Some("/resume"));
        app.press(Key::Char('2'));
        app.press(Key::Char('p'));
        assert_eq!(app.take_control(), None);
    }
}
