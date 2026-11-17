//! Key state for the status screens. The draw loop renders this. It does not
//! read the catalog.

use serde_json::Value;

use super::ingest::{error_detail, ingest_from, peer_detail, Ingest, Local, Session};

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
const PEERS: usize = 2;
const WORK: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    Nav,
    Body,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    None,
    Prepare,
    Reload,
    Measure(usize),
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

pub(crate) struct App {
    pub nav: usize,
    pub view: usize,
    pub focus: Focus,
    pub cursor: usize,
    pub scroll: u16,
    pub ingest: Ingest,
    pub detail: String,
    pub quit: bool,
    pub control: Option<&'static str>,
    pub survey: SurveyCtl,
    pub live_peers: Vec<Session>,
    pub peer_down: bool,
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
            ingest: Ingest::default(),
            detail: String::new(),
            quit: false,
            control: None,
            survey: SurveyCtl::default(),
            live_peers: Vec::new(),
            peer_down: false,
            notice: String::new(),
        }
    }

    pub(crate) fn press(&mut self, key: Key) {
        if self.quit {
            return;
        }
        match key {
            Key::Char('q') => self.quit = true,
            Key::Char(ch @ '1'..='9') => {
                let index = ch as usize - '1' as usize;
                if index < VIEWS.len() {
                    self.jump(index);
                }
            }
            Key::Esc => self.esc(),
            Key::Enter => self.enter(),
            Key::Left | Key::Char('h') => self.left(),
            Key::Right | Key::Char('l') => self.right(),
            Key::Up | Key::Char('k') => self.shift(-1),
            Key::Down | Key::Char('j') => self.shift(1),
            Key::Char('p') if self.view == 0 => {
                if let Some(paused) = self.ingest.paused {
                    self.control = Some(super::ingest::control_path(paused));
                }
            }
            Key::Char('r') if self.view == SURVEY => self.measure_one(),
            Key::Char('R') if self.view == SURVEY => self.measure_all(),
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
        let len = self.list_len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
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

    fn left(&mut self) {
        self.survey.detail = false;
        self.focus = Focus::Nav;
    }

    fn right(&mut self) {
        match self.focus {
            Focus::Nav => self.open_view(self.nav),
            Focus::Body => self.open_detail(),
            Focus::Detail => {}
        }
    }

    fn enter(&mut self) {
        match self.focus {
            Focus::Nav => self.open_view(self.nav),
            Focus::Body => self.open_detail(),
            Focus::Detail => {}
        }
    }

    fn open_detail(&mut self) {
        if self.view == SURVEY {
            if self.survey.items.is_empty() {
                return;
            }
            self.survey.detail = true;
            self.survey.scroll = 0;
            self.focus = Focus::Detail;
            return;
        }
        if self.view == PEERS {
            let Some(session) = self.live_peers.get(self.cursor) else {
                return;
            };
            self.detail = peer_detail(session, &self.live_peers, self.ingest.chain_number);
            self.scroll = 0;
            self.focus = Focus::Detail;
            return;
        }
        if self.view == WORK {
            let Some((count, error)) = self.ingest.errors.get(self.cursor) else {
                return;
            };
            self.detail = error_detail(*count, error);
            self.scroll = 0;
            self.focus = Focus::Detail;
        }
    }

    fn esc(&mut self) {
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
                let len = self.list_len();
                if len == 0 {
                    self.scroll = slide(self.scroll, delta);
                    return;
                }
                self.cursor = move_index(self.cursor, len, delta);
            }
            Focus::Detail if self.view == SURVEY && self.survey.detail => {
                self.survey.scroll = slide(self.survey.scroll, delta);
            }
            Focus::Detail => {
                self.scroll = slide(self.scroll, delta);
            }
        }
    }

    fn list_len(&self) -> usize {
        match self.view {
            PEERS => self.live_peers.len(),
            WORK => self.ingest.errors.len(),
            _ => 0,
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
    Left,
    Right,
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
        let text = super::super::ingest::overview_lines(&app.ingest).join("\n");
        assert!(text.contains("  cursor          5"), "{text}");
        assert!(text.contains("  behind          4"), "{text}");
        assert!(text.contains("Discovery"), "{text}");
        assert!(text.contains("  identities      10"), "{text}");
        assert!(text.contains("  stored          4"), "{text}");
        assert!(text.contains("  missing         2"), "{text}");
        assert!(text.contains("  lens            reading"), "{text}");
        assert!(text.contains("  answers         8"), "{text}");
        assert!(text.contains("  spots           12"), "{text}");
        assert!(text.contains("  mode            following"), "{text}");
        assert!(text.contains("2.0 KB"), "{text}");
        assert!(text.contains("  service         not running"), "{text}");
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
        let text = super::super::ingest::overview_lines(&app.ingest).join("\n");
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
        app.press(Key::Left);
        assert_eq!(app.focus, Focus::Nav);
        assert_eq!(app.view, 1);
        app.press(Key::Right);
        assert_eq!(app.focus, Focus::Body);
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
