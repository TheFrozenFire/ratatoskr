//! Ratatui screens. The left column is the view list. The right pane is the
//! open view, or the detail of the selected row.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
    Frame,
};

use super::ingest::{self, Session};
use super::model::{App, Focus, SurveyRow, PAGE, VIEWS};

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Min(40)])
        .split(area);
    frame.render_widget(nav(app), columns[0]);
    render_main(frame, app, columns[1]);
}

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    if app.focus == Focus::Detail {
        frame.render_widget(detail(app), area);
        return;
    }
    let lines = document(app);
    let focused = app.focus == Focus::Body;
    if let Some(table) = list_table(app, area.width) {
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

fn list_table(app: &App, width: u16) -> Option<Table<'static>> {
    let focused = app.focus == Focus::Body;
    match app.view {
        1 if !app.survey.items.is_empty() => Some(survey_table(app, width, focused)),
        2 if !app.live_peers.is_empty() => Some(peer_table(app, focused)),
        3 if !app.ingest.errors.is_empty() => Some(error_table(app, focused)),
        _ => None,
    }
}

fn survey_table(app: &App, _width: u16, focused: bool) -> Table<'static> {
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
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
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
            Constraint::Min(18),
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

fn error_table(app: &App, focused: bool) -> Table<'static> {
    let rows = app
        .ingest
        .errors
        .iter()
        .enumerate()
        .map(|(index, (count, error))| {
            let row = Row::new(vec![
                Cell::from(count.to_string()),
                Cell::from(error.clone()),
            ]);
            if focused && index == app.cursor {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    Table::new(rows, [Constraint::Length(8), Constraint::Min(20)])
        .header(header(&["count", "error"]))
        .block(pane("", false))
        .column_spacing(1)
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
        return format!("{}\n{}\nnot measured", item.name, item.class);
    }
    format!(
        "{}\nclass           {}\nretained        {}\nvalue in        {}\nvalue out       {}\ncount           {}\ngas             {}\n\n{}",
        item.name, item.class, item.retained, item.value_in, item.value_out, item.count, item.gas, item.text
    )
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
