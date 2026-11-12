//! Full-screen status for the state pipeline. It polls the exporter.

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
    Terminal,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const RATE_WINDOW: Duration = Duration::from_secs(10);

struct Mark {
    at: Instant,
    jobs: Vec<u64>,
}

struct View {
    snap: Option<Value>,
    fetched: Option<Instant>,
    error: String,
    history: VecDeque<Mark>,
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
        history: VecDeque::new(),
        rates: Vec::new(),
    };
    let result = loop {
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('p') => apply_control(&mut view, &url),
                        _ => {}
                    }
                }
            }
        }
        let due = view
            .fetched
            .map(|at| at.elapsed() >= Duration::from_millis(400))
            .unwrap_or(true);
        if due {
            match fetch(&url) {
                Ok(snap) => {
                    view.note(&snap);
                    view.snap = Some(snap);
                    view.fetched = Some(Instant::now());
                    view.error.clear();
                }
                Err(err) => view.error = err,
            }
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
        self.history.push_back(Mark { at: now, jobs });
        while self
            .history
            .front()
            .is_some_and(|mark| now.duration_since(mark.at) > RATE_WINDOW)
        {
            self.history.pop_front();
        }
        let Some(oldest) = self.history.front() else {
            return;
        };
        let secs = now.duration_since(oldest.at).as_secs_f64();
        if secs < 1.0 {
            return;
        }
        self.rates = self
            .history
            .back()
            .map(|latest| {
                latest
                    .jobs
                    .iter()
                    .enumerate()
                    .map(|(index, jobs)| {
                        let prev = oldest.jobs.get(index).copied().unwrap_or(0);
                        jobs.saturating_sub(prev) as f64 / secs
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
}

fn draw(frame: &mut ratatui::Frame, view: &View, url: &str) {
    let lines = job_error_lines(view);
    let error_rows = (lines.len().clamp(1, 6) as u16).saturating_add(2);
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(error_rows),
        ])
        .split(area);
    frame.render_widget(header(view, url), chunks[0]);
    frame.render_widget(catalog(view), chunks[1]);
    frame.render_widget(stages(view), chunks[2]);
    frame.render_widget(queues(view, chunks[3]), chunks[3]);
    frame.render_widget(job_errors(lines), chunks[4]);
}

fn header(view: &View, url: &str) -> Paragraph<'static> {
    let snap = view.snap.as_ref();
    let block = snap.and_then(|snap| snap["block"].as_u64()).unwrap_or(0);
    let uptime = snap
        .and_then(|snap| snap["uptime_ms"].as_u64())
        .unwrap_or(0);
    let link = if view.error.is_empty() {
        view.fetched
            .map(|at| format!("snapshot {:.1}s ago", at.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "snapshot never".into())
    } else {
        format!("exporter down ({url}): {}", view.error)
    };
    let paused = snap
        .and_then(|snap| snap["paused"].as_bool())
        .unwrap_or(false);
    let text = format!(
        " block {block}    up {}    {}{link}\n {}",
        format_uptime(uptime),
        if paused { "paused    " } else { "" },
        pace_line(view)
    );
    Paragraph::new(text).block(
        Block::default()
            .title(format!(
                " ratatoskr state    {}    q quits ",
                control_label(paused)
            ))
            .borders(Borders::ALL),
    )
}

fn apply_control(view: &mut View, url: &str) {
    let paused = view
        .snap
        .as_ref()
        .and_then(|snap| snap["paused"].as_bool())
        .unwrap_or(false);
    match post_control(url, control_path(paused)) {
        Ok(snap) => {
            view.note(&snap);
            view.snap = Some(snap);
            view.fetched = Some(Instant::now());
            view.error.clear();
        }
        Err(err) => view.error = err,
    }
}

fn control_path(paused: bool) -> &'static str {
    if paused {
        "/resume"
    } else {
        "/pause"
    }
}

fn control_label(paused: bool) -> &'static str {
    if paused {
        "p resumes"
    } else {
        "p pauses"
    }
}

fn catalog(view: &View) -> Paragraph<'static> {
    let snap = view.snap.as_ref();
    let num = |field: &str| snap.and_then(|snap| snap[field].as_u64()).unwrap_or(0);
    let text = format!(
        " identities {}   immutable {}   mutable {}   tick pages {}   quotes {}   curves {}   conduits {}   mismatches {}   jobs {}",
        commas(num("identities")),
        commas(num("immutable")),
        commas(num("mutable")),
        commas(num("tick_pages")),
        commas(num("quotes")),
        commas(num("curves")),
        commas(num("conduits")),
        commas(num("mismatches")),
        commas(num("jobs")),
    );
    Paragraph::new(text).block(Block::default().title(" catalog ").borders(Borders::ALL))
}

fn stages(view: &View) -> Table<'static> {
    let identities = identities_of(view);
    let rows = stages_of(view);
    let table_rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(index, stage)| {
            let inflight = stage["inflight"].as_u64().unwrap_or(0);
            let batches = stage["batches"].as_u64().unwrap_or(0);
            let queued = stage["queued"].as_u64().unwrap_or(0);
            let mut style = Style::default();
            if inflight > 0 {
                style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
            }
            let rate = view.rates.get(index).copied().unwrap_or(0.0);
            let last = if batches == 0 {
                "—".into()
            } else {
                format!("{} ms", stage["last_ms"].as_u64().unwrap_or(0))
            };
            let rate_text = if batches == 0 {
                "—".into()
            } else {
                format!("{rate:.1}/s")
            };
            Row::new(vec![
                Cell::from(stage["name"].as_str().unwrap_or("").to_string()),
                num_cell(queued),
                right_cell(percent(queued, identities)),
                num_cell(batches),
                num_cell(stage["jobs"].as_u64().unwrap_or(0)),
                num_cell(inflight),
                right_cell(last),
                right_cell(rate_text),
                Cell::from(spark(&stage["samples_ms"])),
            ])
            .style(style)
        })
        .collect();
    let header = Row::new(vec![
        Cell::from("stage"),
        right_cell("queued"),
        right_cell("left"),
        right_cell("batches"),
        right_cell("jobs"),
        right_cell("now"),
        right_cell("last"),
        right_cell("rate"),
        Cell::from("batch ms"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    Table::new(
        table_rows,
        [
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(Block::default().title(" stages ").borders(Borders::ALL))
}

fn queues(view: &View, area: Rect) -> Paragraph<'static> {
    let identities = identities_of(view);
    let width = area.width.saturating_sub(36).clamp(12, 48) as usize;
    let lines = stages_of(view)
        .iter()
        .map(|stage| {
            let name = stage["name"].as_str().unwrap_or("");
            let queued = stage["queued"].as_u64().unwrap_or(0);
            let inflight = stage["inflight"].as_u64().unwrap_or(0);
            let ratio = ratio(queued, identities);
            let style = if inflight > 0 {
                Style::default().fg(Color::Green)
            } else if queued > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled(format!(" {name:<10}"), style),
                Span::styled(format!(" {:>10}", commas(queued)), style),
                Span::styled(format!(" {:>6}  ", percent(queued, identities)), style),
                Span::styled(bar(ratio, width), style),
            ])
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines).block(
        Block::default()
            .title(" jobs waiting ")
            .borders(Borders::ALL),
    )
}

fn pace_line(view: &View) -> String {
    let stages = stages_of(view);
    let running: Vec<String> = stages
        .iter()
        .enumerate()
        .filter_map(|(index, stage)| {
            if stage["inflight"].as_u64().unwrap_or(0) == 0 {
                return None;
            }
            let name = stage["name"].as_str().unwrap_or("stage");
            let rate = view.rates.get(index).copied().unwrap_or(0.0);
            Some(format!("{name} {rate:.0}/s"))
        })
        .collect();
    if !running.is_empty() {
        return running.join("    ");
    }
    let identities = identities_of(view);
    let active = stages
        .iter()
        .enumerate()
        .find(|(_, stage)| stage["queued"].as_u64().unwrap_or(0) > 0);
    let Some((index, stage)) = active else {
        return "idle".into();
    };
    let name = stage["name"].as_str().unwrap_or("stage");
    let queued = stage["queued"].as_u64().unwrap_or(0);
    let done = identities.saturating_sub(queued);
    let rate = view.rates.get(index).copied().unwrap_or(0.0);
    let eta = if rate >= 1.0 {
        format_duration((queued as f64 / rate) as u64)
    } else {
        "—".into()
    };
    format!(
        "{name} {}/{} done    {} left    {rate:.1}/s    eta {eta}",
        commas(done),
        commas(identities),
        commas(queued)
    )
}

fn job_error_lines(view: &View) -> Vec<Line<'static>> {
    if let Some(snap) = &view.snap {
        let Some(errors) = snap["errors"].as_array() else {
            return vec![Line::from(" none")];
        };
        if errors.is_empty() {
            return vec![Line::from(" none")];
        }
        return errors
            .iter()
            .take(6)
            .map(|error| {
                Line::from(Span::styled(
                    format!(
                        " {:>8}  {}",
                        commas(error["count"].as_u64().unwrap_or(0)),
                        error["error"].as_str().unwrap_or("")
                    ),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect();
    }
    if view.error.is_empty() {
        vec![Line::from(" waiting for the exporter")]
    } else {
        vec![Line::from(" no snapshot yet")]
    }
}

fn job_errors(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().title(" job errors ").borders(Borders::ALL))
}

fn stage_jobs(snap: &Value) -> Vec<u64> {
    snap["stages"]
        .as_array()
        .map(|stages| {
            stages
                .iter()
                .map(|stage| stage["jobs"].as_u64().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

fn identities_of(view: &View) -> u64 {
    view.snap
        .as_ref()
        .and_then(|snap| snap["identities"].as_u64())
        .unwrap_or(0)
}

fn stages_of(view: &View) -> Vec<Value> {
    view.snap
        .as_ref()
        .and_then(|snap| snap["stages"].as_array())
        .cloned()
        .unwrap_or_default()
}

fn num_cell(value: u64) -> Cell<'static> {
    right_cell(commas(value))
}

fn right_cell(text: impl Into<String>) -> Cell<'static> {
    Cell::from(Line::from(text.into()).alignment(Alignment::Right))
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64).clamp(0.0, 1.0)
    }
}

fn percent(part: u64, whole: u64) -> String {
    format!("{:.1}%", ratio(part, whole) * 100.0)
}

fn bar(ratio: f64, width: usize) -> String {
    let filled = ((ratio * width as f64).round() as usize).min(width);
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else if mins > 0 {
        format!("{mins}m {rem:02}s")
    } else {
        format!("{rem}s")
    }
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
    let max = values.iter().copied().max().unwrap_or(1).max(1000);
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
    let mut last = String::new();
    for attempt in 0..2 {
        match fetch_once(url) {
            Ok(value) => return Ok(value),
            Err(err) => {
                last = err;
                if attempt == 0 {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        }
    }
    Err(last)
}

fn fetch_once(url: &str) -> Result<Value, String> {
    let (host, addr, path) = endpoint(url)?;
    let mut stream = connect(&addr)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|err| err.to_string())?;
    let (code, body) = read_response(&mut stream)?;
    if code != 200 {
        return Err(format!("status returned {code}"));
    }
    serde_json::from_str(&body).map_err(|err| format!("{err}"))
}

fn post_control(status_url: &str, path: &str) -> Result<Value, String> {
    let (host, addr, _status_path) = endpoint(status_url)?;
    let mut stream = connect(&addr)?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|err| err.to_string())?;
    let (code, body) = read_response(&mut stream)?;
    if code != 200 {
        return Err(format!("control {path} returned {code}"));
    }
    serde_json::from_str(&body).map_err(|err| format!("{err}"))
}

fn endpoint(url: &str) -> Result<(String, String, String), String> {
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
    Ok((host.to_string(), addr, path))
}

fn connect(addr: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|err| format!("{addr}: {err}"))?,
        Duration::from_millis(400),
    )
    .map_err(|err| err.to_string())?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(400)))
        .ok();
    Ok(stream)
}

fn read_response(stream: &mut TcpStream) -> Result<(u16, String), String> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    loop {
        if let Some(response) = complete_response(&buf) {
            return Ok(response);
        }
        let n = stream.read(&mut tmp).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1_000_000 {
            return Err("status response too large".into());
        }
    }
    complete_response(&buf).ok_or_else(|| "truncated status response".into())
}

fn complete_response(buf: &[u8]) -> Option<(u16, String)> {
    let split = buf.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(buf.get(..split)?).ok()?;
    let code = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let length = head.lines().find_map(|line| {
        let value = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))?
            .1
            .trim();
        value.parse::<usize>().ok()
    })?;
    let start = split + 4;
    let body = buf.get(start..start + length)?;
    Some((code, String::from_utf8(body.to_vec()).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p_pauses_until_the_snapshot_says_paused() {
        assert_eq!(control_path(false), "/pause");
        assert_eq!(control_label(false), "p pauses");
        assert_eq!(control_path(true), "/resume");
        assert_eq!(control_label(true), "p resumes");
    }
}
