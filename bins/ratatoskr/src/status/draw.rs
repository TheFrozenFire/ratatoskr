//! Ratatui screens. The left column is the view list. The right pane is the
//! open view, or the detail of the selected row.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Widget, Wrap},
    Frame,
};

use super::ingest::{self, Session, StageRow};
use super::model::{App, Focus, SurveyRow, PAGE, VIEWS};

const SURVEY_HINT: &str = "r measure    R all    enter detail    esc back";

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Min(40)])
        .split(area);
    frame.render_widget(nav(app), columns[0]);
    render_main(frame, app, columns[1]);
}

fn render_main(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.focus == Focus::Detail {
        let (body, hint) = hint_split(app.view == 1, area);
        app.page_rows = body.height.saturating_sub(2).max(1);
        frame.render_widget(detail(app), body);
        paint_hint(frame, hint);
        return;
    }
    match app.view {
        0 => {
            app.page_rows = area.height.max(1);
            let bands = ingestion_bands(app);
            render_bands(frame, app, area, bands);
        }
        1 => render_survey(frame, app, area),
        3 => {
            app.page_rows = area.height.max(1);
            let bands = work_bands(app);
            render_bands(frame, app, area, bands);
        }
        _ => render_document(frame, app, area),
    }
}

fn render_document(frame: &mut Frame, app: &mut App, area: Rect) {
    let lines = document(app);
    let focused = app.focus == Focus::Body;
    app.page_rows = area.height.saturating_sub(2).max(1);
    if let Some(table) = list_table(app) {
        let summary = (lines.len() as u16 + 2)
            .min(area.height.saturating_sub(6))
            .max(3);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(summary), Constraint::Min(3)])
            .split(area);
        frame.render_widget(text_block(&lines, VIEWS[app.view], focused, 0), rows[0]);
        frame.render_widget(table, rows[1]);
        return;
    }
    frame.render_widget(
        text_block(&lines, VIEWS[app.view], focused, app.scroll),
        area,
    );
}

fn render_survey(frame: &mut Frame, app: &mut App, area: Rect) {
    let (body, hint) = hint_split(true, area);
    let lines = survey_lines(app);
    let focused = app.focus == Focus::Body;
    if app.survey.items.is_empty() {
        app.page_rows = body.height.saturating_sub(2).max(1);
        frame.render_widget(text_block(&lines, VIEWS[1], focused, 0), body);
    } else {
        let summary = (lines.len() as u16 + 2)
            .min(body.height.saturating_sub(6))
            .max(3);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(summary), Constraint::Min(3)])
            .split(body);
        app.page_rows = rows[1].height.saturating_sub(3).max(1);
        frame.render_widget(text_block(&lines, VIEWS[1], focused, 0), rows[0]);
        frame.render_widget(survey_table(app, focused), rows[1]);
    }
    paint_hint(frame, hint);
}

fn hint_split(survey: bool, area: Rect) -> (Rect, Option<Rect>) {
    if !survey || area.height < 4 {
        return (area, None);
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    (rows[0], Some(rows[1]))
}

fn paint_hint(frame: &mut Frame, area: Option<Rect>) {
    let Some(area) = area else {
        return;
    };
    frame.render_widget(
        Paragraph::new(SURVEY_HINT).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn document(app: &App) -> Vec<String> {
    match app.view {
        0 => ingest::overview_lines(&app.ingest),
        1 => survey_lines(app),
        2 => {
            let mut lines = ingest::peer_lines(&app.ingest, &app.live_peers, app.peer_down);
            if !app.notice.is_empty() {
                lines.push(app.notice.clone());
            }
            lines
        }
        3 => ingest::work_lines(&app.ingest),
        4 => ingest::pool_lines(&app.ingest),
        5 => ingest::conduit_lines(&app.ingest),
        6 => ingest::token_lines(&app.ingest),
        7 => ingest::feed_lines(&app.ingest),
        _ => ingest::spot_lines(&app.ingest),
    }
}

fn survey_lines(app: &App) -> Vec<String> {
    let items = &app.survey.items;
    let mut lines = vec!["Survey".into(), fact("status", &app.survey.note)];
    lines.push(fact("scenarios", &items.len().to_string()));
    let measured = items.iter().filter(|item| !item.text.is_empty()).count();
    lines.push(fact("measured", &measured.to_string()));
    for class in ["ordinary", "long", "via", "deep", "isolated"] {
        let count = items.iter().filter(|item| item.class == class).count();
        if count > 0 {
            lines.push(fact(class, &count.to_string()));
        }
    }
    lines
}

fn list_table(app: &App) -> Option<Table<'static>> {
    let focused = app.focus == Focus::Body;
    match app.view {
        2 if !app.live_peers.is_empty() => Some(peer_table(app, focused)),
        _ => None,
    }
}

fn survey_table(app: &App, focused: bool) -> Table<'static> {
    let range = window(app.survey.items.len(), app.survey.cursor);
    let rows = app.survey.items[range.clone()]
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            let row = Row::new(vec![
                Cell::from(item.name.clone()),
                Cell::from(item.class.clone()),
                Cell::from(item.retained.clone()),
                Cell::from(item.count.clone()),
                Cell::from(item.gas.clone()),
            ]);
            if focused && range.start + offset == app.survey.cursor {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header(&["scenario", "class", "retained", "count", "gas"]))
    .block(pane("", false))
    .column_spacing(1)
}

fn peer_table(app: &App, focused: bool) -> Table<'static> {
    let best = app
        .live_peers
        .iter()
        .filter_map(|session| session.head)
        .max();
    let rows = app
        .live_peers
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let row = Row::new(session_cells(session, best));
            if focused && index == app.cursor {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(10),
        ],
    )
    .header(header(&[
        "peer", "eth", "head", "behind", "earliest", "fetch",
    ]))
    .block(pane("", false))
    .column_spacing(1)
}

fn session_cells(session: &Session, best: Option<u64>) -> Vec<Cell<'static>> {
    let behind = match (session.head, best) {
        (Some(head), Some(best)) => best.saturating_sub(head).to_string(),
        _ => "—".into(),
    };
    let fetch = match (session.head, best) {
        (Some(head), Some(best)) if head == best => "preferred",
        (Some(_), Some(_)) => "later",
        _ => "no head",
    };
    vec![
        Cell::from(session.label.clone()),
        Cell::from(session.eth.to_string()),
        Cell::from(
            session
                .head
                .map(|head| head.to_string())
                .unwrap_or_else(|| "—".into()),
        ),
        Cell::from(behind),
        Cell::from(
            session
                .earliest
                .map(|block| block.to_string())
                .unwrap_or_else(|| "—".into()),
        ),
        Cell::from(fetch),
    ]
}

fn detail(app: &App) -> Paragraph<'static> {
    let text = if app.view == 1 {
        app.survey
            .items
            .get(app.survey.cursor)
            .map(survey_detail)
            .unwrap_or_default()
    } else {
        app.detail.clone()
    };
    let scroll = if app.view == 1 {
        app.survey.scroll
    } else {
        app.scroll
    };
    Paragraph::new(text)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .block(pane(VIEWS[app.view], true))
}

fn survey_detail(item: &SurveyRow) -> String {
    if item.text.is_empty() {
        return format!(
            "{}\n{}\nnot measured\n\n{SURVEY_HINT}",
            item.name, item.class
        );
    }
    format!(
        "{}\nclass           {}\nretained        {}\nvalue in        {}\nvalue out       {}\ncount           {}\ngas             {}\n\n{}",
        item.name, item.class, item.retained, item.value_in, item.value_out, item.count, item.gas, item.text
    )
}

struct Card {
    title: &'static str,
    lines: Vec<String>,
}

enum Band {
    Cards(Vec<Card>),
    Text {
        title: &'static str,
        lines: Vec<String>,
    },
    Discovery {
        facts: Vec<String>,
        lags: Vec<(String, u64, Option<u64>)>,
    },
    Stages(Vec<StageRow>),
    Coverage(Vec<(String, String, String)>),
    Errors {
        rows: Vec<(u64, String)>,
        cursor: Option<usize>,
    },
}

fn ingestion_bands(app: &App) -> Vec<Band> {
    let ingest = &app.ingest;
    let mut bands = vec![
        Band::Cards(vec![
            Card {
                title: "process",
                lines: ingest::process_lines(ingest),
            },
            Card {
                title: "headers",
                lines: card_body(ingest::header_lines(ingest)),
            },
            Card {
                title: "corpus",
                lines: card_body(ingest::corpus_lines(ingest)),
            },
        ]),
        Band::Cards(vec![
            Card {
                title: "tokens",
                lines: card_body(ingest::token_lines(ingest)),
            },
            Card {
                title: "feeds",
                lines: card_body(ingest::feed_lines(ingest)),
            },
            Card {
                title: "spots",
                lines: card_body(ingest::spot_lines(ingest)),
            },
            Card {
                title: "peers",
                lines: ingest::peer_card(ingest),
            },
        ]),
        Band::Discovery {
            facts: ingest::discovery_facts(ingest),
            lags: ingest::furthest_lags(ingest),
        },
    ];
    if !ingest.stages.is_empty() {
        bands.push(Band::Stages(ingest.stages.clone()));
    }
    let queue = ingest::queue_lines(ingest);
    if !queue.is_empty() {
        bands.push(Band::Text {
            title: "queue",
            lines: queue,
        });
    }
    bands
}

fn work_bands(app: &App) -> Vec<Band> {
    let mut bands = vec![Band::Coverage(ingest::coverage_rows(&app.ingest))];
    if !app.ingest.stages.is_empty() {
        bands.push(Band::Stages(app.ingest.stages.clone()));
    }
    bands.push(Band::Text {
        title: "stuck",
        lines: ingest::stuck_lines(&app.ingest),
    });
    if !app.ingest.errors.is_empty() {
        let cursor = (app.focus == Focus::Body).then_some(app.cursor);
        bands.push(Band::Errors {
            rows: app.ingest.errors.clone(),
            cursor,
        });
    }
    bands
}

fn card_body(mut lines: Vec<String>) -> Vec<String> {
    if lines
        .first()
        .is_some_and(|line| !line.starts_with("  ") && !line.is_empty())
    {
        lines.remove(0);
    }
    lines.into_iter().filter(|line| !line.is_empty()).collect()
}

fn render_bands(frame: &mut Frame, app: &mut App, area: Rect, bands: Vec<Band>) {
    let heights: Vec<u16> = bands.iter().map(band_height).collect();
    let total: u16 = heights.iter().copied().sum();
    let max_scroll = total.saturating_sub(area.height);
    if app.scroll > max_scroll {
        app.scroll = max_scroll;
    }
    let focused = app.focus == Focus::Body;
    let scroll = app.scroll as i32;
    frame.render_widget(Clear, area);
    let mut top = 0i32;
    for (band, height) in bands.iter().zip(heights) {
        blit(frame, area, band, height, top, scroll, focused);
        top += height as i32;
    }
}

fn blit(
    frame: &mut Frame,
    view: Rect,
    band: &Band,
    height: u16,
    top: i32,
    scroll: i32,
    focused: bool,
) {
    if height == 0 || view.width == 0 {
        return;
    }
    let mut buf = Buffer::empty(Rect::new(0, 0, view.width, height));
    render_band(&mut buf, band, Rect::new(0, 0, view.width, height), focused);
    let frame_buf = frame.buffer_mut();
    for row in 0..height {
        let screen = top + row as i32 - scroll;
        if screen < 0 || screen >= view.height as i32 {
            continue;
        }
        for col in 0..view.width {
            frame_buf[(view.x + col, view.y + screen as u16)] = buf[(col, row)].clone();
        }
    }
}

fn render_band(buf: &mut Buffer, band: &Band, area: Rect, focused: bool) {
    match band {
        Band::Cards(cards) => {
            let constraints = vec![Constraint::Fill(1); cards.len()];
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area);
            for (card, rect) in cards.iter().zip(cols.iter()) {
                lines_block(&card.lines, card.title, focused).render(*rect, buf);
            }
        }
        Band::Text { title, lines } => lines_block(lines, title, focused).render(area, buf),
        Band::Discovery { facts, lags } => render_discovery(buf, area, facts, lags, focused),
        Band::Stages(stages) => stage_widget(stages, focused).render(area, buf),
        Band::Coverage(rows) => coverage_widget(rows, focused).render(area, buf),
        Band::Errors { rows, cursor } => error_widget(rows, *cursor, focused).render(area, buf),
    }
}

fn render_discovery(
    buf: &mut Buffer,
    area: Rect,
    facts: &[String],
    lags: &[(String, u64, Option<u64>)],
    focused: bool,
) {
    let block = pane("discovery", focused);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let fact_h = (facts.len() as u16).min(inner.height);
    Paragraph::new(facts.iter().cloned().map(Line::from).collect::<Vec<_>>()).render(
        Rect {
            height: fact_h,
            ..inner
        },
        buf,
    );
    if lags.is_empty() || inner.height <= fact_h {
        return;
    }
    lag_table(lags).render(
        Rect {
            y: inner.y + fact_h,
            height: inner.height - fact_h,
            ..inner
        },
        buf,
    );
}

fn band_height(band: &Band) -> u16 {
    match band {
        Band::Cards(cards) => {
            let lines = cards
                .iter()
                .map(|card| card.lines.len())
                .max()
                .unwrap_or(1)
                .max(1);
            lines as u16 + 2
        }
        Band::Text { lines, .. } => lines.len().max(1) as u16 + 2,
        Band::Discovery { facts, lags } => {
            let mut height = facts.len().max(1) as u16 + 2;
            if !lags.is_empty() {
                height += 1 + lags.len() as u16;
            }
            height
        }
        Band::Stages(stages) => 3 + stages.len().max(1) as u16,
        Band::Coverage(rows) => 3 + rows.len().max(1) as u16,
        Band::Errors { rows, .. } => 3 + rows.len().max(1) as u16,
    }
}

fn lines_block(lines: &[String], title: &str, focused: bool) -> Paragraph<'static> {
    Paragraph::new(lines.iter().cloned().map(Line::from).collect::<Vec<_>>())
        .wrap(Wrap { trim: false })
        .block(pane(title, focused))
}

fn stage_widget(stages: &[StageRow], focused: bool) -> Table<'static> {
    let rows = stages
        .iter()
        .map(|stage| {
            Row::new(vec![
                Cell::from(stage.name.clone()),
                Cell::from(grouped(&stage.queued)),
                Cell::from(grouped(&stage.inflight)),
                Cell::from(grouped(&stage.jobs)),
                Cell::from(grouped(&stage.batches)),
                Cell::from(ingest::shown(&stage.last)),
                Cell::from(ingest::shown(&stage.rate)),
            ])
        })
        .collect::<Vec<_>>();
    Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(12),
            Constraint::Length(10),
        ],
    )
    .header(header(&[
        "stage", "waiting", "running", "finished", "batches", "last", "rate",
    ]))
    .block(pane("state", focused))
    .column_spacing(2)
}

fn coverage_widget(rows: &[(String, String, String)], focused: bool) -> Table<'static> {
    let body = rows
        .iter()
        .map(|(name, stored, short)| {
            Row::new(vec![
                Cell::from(name.clone()),
                Cell::from(stored.clone()),
                Cell::from(short.clone()),
            ])
        })
        .collect::<Vec<_>>();
    Table::new(
        body,
        [
            Constraint::Length(14),
            Constraint::Fill(1),
            Constraint::Fill(1),
        ],
    )
    .header(header(&["", "stored", "short"]))
    .block(pane("coverage", focused))
    .column_spacing(2)
}

fn lag_table(lags: &[(String, u64, Option<u64>)]) -> Table<'static> {
    let rows = lags
        .iter()
        .map(|(name, block, behind)| {
            Row::new(vec![
                Cell::from(name.clone()),
                Cell::from(ingest::commas(*block)),
                Cell::from(ingest::num(*behind)),
            ])
        })
        .collect::<Vec<_>>();
    Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(14),
            Constraint::Length(12),
        ],
    )
    .header(header(&["furthest behind", "block", "behind"]))
    .column_spacing(2)
}

fn error_widget(rows: &[(u64, String)], cursor: Option<usize>, focused: bool) -> Table<'static> {
    let body = rows
        .iter()
        .enumerate()
        .map(|(index, (count, error))| {
            let row = Row::new(vec![
                Cell::from(ingest::commas(*count)),
                Cell::from(error.clone()),
            ]);
            if cursor == Some(index) {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    Table::new(body, [Constraint::Length(10), Constraint::Fill(1)])
        .header(header(&["count", "error"]))
        .block(pane("errors", focused))
        .column_spacing(2)
}

fn grouped(text: &str) -> String {
    match text.parse::<u64>() {
        Ok(value) => ingest::commas(value),
        Err(_) => ingest::shown(text),
    }
}

fn nav(app: &App) -> Paragraph<'static> {
    let lines = VIEWS
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mark = if index == app.view { "▸ " } else { "  " };
            let style = if app.focus == Focus::Nav && index == app.nav {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if index == app.view {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("{mark}{name}"), style))
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines).block(pane("views", app.focus == Focus::Nav))
}

fn text_block(lines: &[String], title: &str, focused: bool, scroll: u16) -> Paragraph<'static> {
    Paragraph::new(lines.iter().cloned().map(Line::from).collect::<Vec<_>>())
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .block(pane(title, focused))
}

fn header(names: &'static [&str]) -> Row<'static> {
    Row::new(names.iter().copied().map(Cell::from).collect::<Vec<_>>())
        .style(Style::default().add_modifier(Modifier::BOLD))
}

fn pane(title: &str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" {title} ")
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

fn fact(label: &str, value: &str) -> String {
    format!("  {label:<16}{value}")
}

fn window(len: usize, cursor: usize) -> std::ops::Range<usize> {
    if len <= PAGE {
        return 0..len;
    }
    let start = cursor.saturating_sub(PAGE / 2).min(len - PAGE);
    start..start + PAGE
}
