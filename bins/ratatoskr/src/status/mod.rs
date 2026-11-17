//! Status screens. The process reads the live files and the exporter. It does
//! not open a writer.

mod draw;
mod ingest;
mod load;
mod model;
mod net;

use std::collections::VecDeque;
use std::io::{self, stdout};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use model::{App, Key, Order, SurveyRow};

pub fn run(
    base: &str,
    rate_window: Duration,
    data_dir: &Path,
    caches: &config::Store,
    spots: &config::Spots,
    socket: &Path,
) -> eyre::Result<()> {
    let url = net::status_url(base);
    let shelf = store::service::Shelf::open(data_dir, caches);
    let (jobs, job_rx) = mpsc::channel();
    let (reports_tx, reports) = mpsc::channel();
    let dir = data_dir.to_path_buf();
    let caches = caches.clone();
    let spots = spots.clone();
    thread::spawn(move || survey_thread(job_rx, reports_tx, dir, caches, spots));
    let mut screen = Screen::enter()?;
    let mut app = App::new();
    let mut names = load::Names::new();
    let mut browser = load::Browser::new();
    let mut rates = Pace::new(rate_window);
    let mut exporter_at = Instant::now() - Duration::from_secs(1);
    let mut shelf_at = Instant::now() - Duration::from_secs(1);
    let result = loop {
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if let Some(key) = key_of(key.code) {
                        app.press(key);
                    }
                }
            }
        }
        if app.quit {
            break Ok(());
        }
        while let Ok(report) = reports.try_recv() {
            apply_report(&mut app, report);
        }
        dispatch(&app_jobs(&mut app), &jobs);
        if let Some(path) = app.take_control() {
            match net::post_control(&url, path) {
                Ok(snap) => {
                    let pace = rates.note(&snap);
                    let local = load::local_of(&shelf, true);
                    app.apply_status(Some(&snap), &local, &pace);
                }
                Err(err) => app.notice = err,
            }
        }
        if shelf_at.elapsed() >= Duration::from_secs(1) {
            shelf_at = Instant::now();
            app.sightings = shelf
                .peers
                .as_ref()
                .and_then(|peers| peers.sightings().ok());
            if app.view == 2 {
                match load::live_peers(socket) {
                    Ok(lines) => {
                        app.live_peers = lines;
                        app.peer_down = false;
                    }
                    Err(_) => {
                        app.live_peers.clear();
                        app.peer_down = true;
                    }
                }
            }
            if !matches!(app.view, 0 | 1) {
                browser.turn(&mut app);
                load::refresh(&mut app, &shelf, &mut names, &browser.after);
            }
        }
        load::fill_detail(&mut app, &shelf, &mut names);
        if exporter_at.elapsed() >= Duration::from_millis(400) {
            exporter_at = Instant::now();
            match net::fetch(&url) {
                Ok(snap) => {
                    let pace = rates.note(&snap);
                    let local = load::local_of(&shelf, true);
                    app.apply_status(Some(&snap), &local, &pace);
                }
                Err(_) => {
                    let local = load::local_of(&shelf, false);
                    app.apply_status(None, &local, &[]);
                }
            }
        }
        screen.terminal.draw(|frame| draw::draw(frame, &app))?;
    };
    drop(screen);
    drop(jobs);
    result
}

fn app_jobs(app: &mut App) -> Vec<Order> {
    let order = app.take_order();
    if order == Order::None {
        Vec::new()
    } else {
        vec![order]
    }
}

fn dispatch(orders: &[Order], jobs: &Sender<Job>) {
    for order in orders {
        let job = match order {
            Order::None => continue,
            Order::Prepare => Job::Prepare,
            Order::Reload => Job::Reload,
            Order::Measure(index) => Job::Measure(*index),
        };
        if jobs.send(job).is_err() {
            break;
        }
    }
}

fn apply_report(app: &mut App, report: Report) {
    match report {
        Report::Ready(items) => {
            let rows = items
                .into_iter()
                .map(|item| SurveyRow {
                    name: item.name,
                    class: item.class.to_string(),
                    ..SurveyRow::default()
                })
                .collect();
            app.survey_prepared(rows);
        }
        Report::Measured { index, text } => app.survey_measured(index, text),
        Report::Failed(err) => app.survey_failed(err),
    }
}

enum Job {
    Prepare,
    Reload,
    Measure(usize),
}

enum Report {
    Ready(Vec<survey::Item>),
    Measured { index: usize, text: String },
    Failed(String),
}

fn survey_thread(
    jobs: Receiver<Job>,
    reports: Sender<Report>,
    dir: std::path::PathBuf,
    caches: config::Store,
    spots: config::Spots,
) {
    let mut session = None;
    while let Ok(job) = jobs.recv() {
        let report = match job {
            Job::Prepare => stage(&mut session, &dir, &caches, &spots, false),
            Job::Reload => stage(&mut session, &dir, &caches, &spots, true),
            Job::Measure(index) => match session.as_mut() {
                Some(session) => match session.measure(index) {
                    Ok(text) => Report::Measured { index, text },
                    Err(err) => Report::Failed(err.to_string()),
                },
                None => Report::Failed("survey is not prepared".into()),
            },
        };
        if reports.send(report).is_err() {
            break;
        }
    }
}

fn stage(
    session: &mut Option<survey::Session>,
    dir: &Path,
    caches: &config::Store,
    spots: &config::Spots,
    reload: bool,
) -> Report {
    let opened = if reload {
        if let Some(existing) = session.as_mut() {
            match existing.reload() {
                Ok(()) => Ok(existing.items()),
                Err(err) => Err(err),
            }
        } else {
            open_session(dir, caches, spots).map(|opened| {
                let items = opened.items();
                *session = Some(opened);
                items
            })
        }
    } else if let Some(existing) = session.as_ref() {
        Ok(existing.items())
    } else {
        open_session(dir, caches, spots).map(|opened| {
            let items = opened.items();
            *session = Some(opened);
            items
        })
    };
    match opened {
        Ok(items) => Report::Ready(items),
        Err(err) => Report::Failed(err.to_string()),
    }
}

fn open_session(
    dir: &Path,
    caches: &config::Store,
    spots: &config::Spots,
) -> eyre::Result<survey::Session> {
    survey::Session::open(dir, spots, caches)
}

struct Pace {
    history: VecDeque<(Instant, Vec<u64>)>,
    window: Duration,
}

impl Pace {
    fn new(window: Duration) -> Self {
        Self {
            history: VecDeque::new(),
            window,
        }
    }

    fn note(&mut self, snap: &serde_json::Value) -> Vec<f64> {
        let now = Instant::now();
        let jobs = snap["stages"]
            .as_array()
            .map(|stages| {
                stages
                    .iter()
                    .map(|stage| stage["jobs"].as_u64().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.history.push_back((now, jobs));
        while self
            .history
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > self.window)
        {
            self.history.pop_front();
        }
        let Some(oldest) = self.history.front() else {
            return Vec::new();
        };
        let secs = now.duration_since(oldest.0).as_secs_f64();
        if secs < 1.0 {
            return Vec::new();
        }
        self.history
            .back()
            .map(|(_, latest)| {
                latest
                    .iter()
                    .enumerate()
                    .map(|(index, jobs)| {
                        let prev = oldest.1.get(index).copied().unwrap_or(0);
                        jobs.saturating_sub(prev) as f64 / secs
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl Screen {
    fn enter() -> eyre::Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(out))?;
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn key_of(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::Char(ch) => Some(Key::Char(ch)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        _ => None,
    }
}
