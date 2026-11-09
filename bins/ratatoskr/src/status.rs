//! Full-screen status for the state pipeline. It polls the exporter.

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Gauge, Paragraph, Row, Table, Wrap},
    Terminal,
};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

struct View {
    snap: Option<Value>,
    fetched: Option<Instant>,
    error: String,
    prev_jobs: Vec<(String, u64)>,
    prev_at: Option<Instant>,
    rates: Vec<f64>,
}

pub fn run(base: &str) -> eyre::Result<()> {
    let url = status_url(base);
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut view = View {
        snap: None,
        fetched: None,
        error: String::new(),
        prev_jobs: Vec::new(),
        prev_at: None,
        rates: Vec::new(),
    };
    let result = loop {
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                {
                    break Ok(());
                }
            }
        }
        match fetch(&url) {
            Ok(snap) => {
                view.note(&snap);
                view.snap = Some(snap);
                view.fetched = Some(Instant::now());
                view.error.clear();
            }
            Err(err) => view.error = err,
        }
        terminal.draw(|frame| draw(frame, &view, &url))?;
    };
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

impl View {
    fn note(&mut self, snap: &Value) {
        let now = Instant::now();
        let jobs = stage_jobs(snap);
        if let Some(prev_at) = self.prev_at {
            let secs = now.duration_since(prev_at).as_secs_f64();
            self.rates = jobs
                .iter()
                .enumerate()
                .map(|(index, (_, jobs))| {
                    let prev = self
                        .prev_jobs
                        .get(index)
                        .map(|(_, jobs)| *jobs)
                        .unwrap_or(*jobs);
                    if secs <= 0.0 {
                        0.0
                    } else {
                        jobs.saturating_sub(prev) as f64 / secs
                    }
                })
                .collect();
        }
        self.prev_jobs = jobs;
        self.prev_at = Some(now);
    }
}

fn draw(frame: &mut ratatui::Frame, view: &View, url: &str) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(9),
            Constraint::Length(3),
            Constraint::Length(8),
        ])
        .split(area);
    frame.render_widget(header(view), chunks[0]);
    frame.render_widget(catalog(view), chunks[1]);
    frame.render_widget(stages(view), chunks[2]);
    frame.render_widget(active_bar(view), chunks[3]);
    frame.render_widget(errors(view, url), chunks[4]);
}

fn header(view: &View) -> Paragraph<'static> {
    let snap = view.snap.as_ref();
    let block = snap.and_then(|snap| snap["block"].as_u64()).unwrap_or(0);
    let uptime = snap
        .and_then(|snap| snap["uptime_ms"].as_u64())
        .unwrap_or(0);
    let age = view
        .fetched
        .map(|at| format!("{:.1}s ago", at.elapsed().as_secs_f64()))
        .unwrap_or_else(|| "never".into());
    let text = format!(
        " block {block}    up {}    snapshot {age}",
        format_uptime(uptime)
    );
    Paragraph::new(text).block(
        Block::default()
            .title(" ratatoskr state ")
            .borders(Borders::ALL),
    )
}

fn catalog(view: &View) -> Paragraph<'static> {
    let snap = view.snap.as_ref();
    let num = |field: &str| snap.and_then(|snap| snap[field].as_u64()).unwrap_or(0);
    let text = format!(
        " identities {}   immutable {}   mutable {}   tick pages {}   quotes {}   curves {}   mismatches {}   jobs {}",
        commas(num("identities")),
        commas(num("immutable")),
        commas(num("mutable")),
        commas(num("tick_pages")),
        commas(num("quotes")),
        commas(num("curves")),
        commas(num("mismatches")),
        commas(num("jobs")),
    );
    Paragraph::new(text).block(Block::default().title(" catalog ").borders(Borders::ALL))
}

fn stages(view: &View) -> Table<'static> {
    let rows = view
        .snap
        .as_ref()
        .and_then(|snap| snap["stages"].as_array())
        .cloned()
        .unwrap_or_default();
    let table_rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(index, stage)| {
            let inflight = stage["inflight"].as_u64().unwrap_or(0);
            let mut style = Style::default();
            if inflight > 0 {
                style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
            }
            let rate = view.rates.get(index).copied().unwrap_or(0.0);
            Row::new(vec![
                stage["name"].as_str().unwrap_or("").to_string(),
                commas(stage["queued"].as_u64().unwrap_or(0)),
                commas(stage["stored"].as_u64().unwrap_or(0)),
                commas(stage["batches"].as_u64().unwrap_or(0)),
                commas(stage["jobs"].as_u64().unwrap_or(0)),
                inflight.to_string(),
                format!("{} ms", stage["last_ms"].as_u64().unwrap_or(0)),
                format!("{rate:.1}/s"),
                spark(&stage["samples_ms"]),
            ])
            .style(style)
        })
        .collect();
    let header = Row::new([
        "stage", "queued", "stored", "batches", "jobs", "now", "last", "rate", "recent",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    Table::new(
        table_rows,
        [
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(Block::default().title(" stages ").borders(Borders::ALL))
}

fn active_bar(view: &View) -> Gauge<'static> {
    let snap = view.snap.as_ref();
    let identities = snap
        .and_then(|snap| snap["identities"].as_u64())
        .unwrap_or(0);
    let stages = snap
        .and_then(|snap| snap["stages"].as_array())
        .cloned()
        .unwrap_or_default();
    let active = stages
        .iter()
        .find(|stage| stage["inflight"].as_u64().unwrap_or(0) > 0);
    let (label, ratio) = match active {
        Some(stage) => {
            let name = stage["name"].as_str().unwrap_or("stage").to_string();
            let stored = stage["stored"].as_u64().unwrap_or(0);
            let ratio = if identities == 0 {
                0.0
            } else {
                (stored as f64 / identities as f64).clamp(0.0, 1.0)
            };
            (format!(" {name}  {stored}/{identities} stored"), ratio)
        }
        None => (" idle".into(), 0.0),
    };
    Gauge::default()
        .block(
            Block::default()
                .title(" active stage ")
                .borders(Borders::ALL),
        )
        .gauge_style(Style::default().fg(Color::Cyan))
        .label(label)
        .ratio(ratio)
}

fn errors(view: &View, url: &str) -> Paragraph<'static> {
    let mut lines = Vec::new();
    if !view.error.is_empty() {
        lines.push(format!(" exporter: {}", view.error));
    }
    if let Some(snap) = &view.snap {
        if let Some(errors) = snap["errors"].as_array() {
            if errors.is_empty() {
                lines.push(" no stored job errors".into());
            }
            for error in errors.iter().take(5) {
                lines.push(format!(
                    " {:>8}  {}",
                    commas(error["count"].as_u64().unwrap_or(0)),
                    error["error"].as_str().unwrap_or("")
                ));
            }
        }
    } else if view.error.is_empty() {
        lines.push(" waiting for the exporter".into());
    }
    lines.push(format!(" {url}    q quits"));
    Paragraph::new(lines.join("\n"))
        .wrap(Wrap { trim: false })
        .block(Block::default().title(" errors ").borders(Borders::ALL))
}

fn stage_jobs(snap: &Value) -> Vec<(String, u64)> {
    snap["stages"]
        .as_array()
        .map(|stages| {
            stages
                .iter()
                .map(|stage| {
                    (
                        stage["name"].as_str().unwrap_or("").to_string(),
                        stage["jobs"].as_u64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn spark(samples: &Value) -> String {
    const BARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let values = samples
        .as_array()
        .map(|samples| {
            samples
                .iter()
                .filter_map(|value| value.as_u64())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let max = values.iter().copied().max().unwrap_or(1).max(1);
    values
        .iter()
        .map(|value| {
            let index = ((*value * 7) / max) as usize;
            BARS[index.min(7)]
        })
        .collect()
}

fn commas(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_uptime(ms: u64) -> String {
    let secs = ms / 1000;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    format!("{hours}h {mins:02}m {rem:02}s")
}

fn status_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/status") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/status")
    }
}

fn fetch(url: &str) -> Result<Value, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "status url needs http://".to_string())?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, "status"));
    let path = format!("/{path}");
    let addr = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|err| format!("{addr}: {err}"))?,
        Duration::from_millis(400),
    )
    .map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(400)))
        .ok();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|err| err.to_string())?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|err| err.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    serde_json::from_str(body).map_err(|err| format!("{err}: {body}"))
}
