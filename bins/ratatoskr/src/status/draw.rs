//! Ratatui screens. The left column is the view list. The right pane is the
//! open view, or the detail of the selected row.

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
    Frame,
};

use super::ingest::control_label;
use super::model::{App, Focus, VIEWS};

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(16), Constraint::Min(40)])
        .split(area);
    frame.render_widget(nav(app), columns[0]);
    if app.focus == Focus::Detail {
        frame.render_widget(detail(app), columns[1]);
    } else if app.view == 0 {
        frame.render_widget(ingestion(app), columns[1]);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(columns[1]);
        frame.render_widget(body(app, rows[0].width), rows[0]);
        frame.render_widget(footer(app), rows[1]);
    }
}

fn nav(app: &App) -> Paragraph<'static> {
    let lines = VIEWS
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mark = if index == app.view { "▸ " } else { "  " };
            let style = if index == app.nav {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("{mark}{name}"), style))
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines).block(Block::default().title(" views ").borders(Borders::ALL))
}

fn ingestion(app: &App) -> Paragraph<'static> {
    let paused = app.ingest.paused.unwrap_or(false);
    let mut lines = Vec::new();
    if app.ingest.exporter_up {
        let pause = if paused { "paused" } else { "running" };
        lines.push(Line::from(format!(
            "block {}    up {}    {pause}",
            app.ingest.block, app.ingest.uptime
        )));
    } else {
        lines.push(Line::from(app.ingest.notice.clone()));
    }
    lines.extend(app.ingest.pipelines.iter().cloned().map(Line::from));
    lines.push(Line::from(app.ingest.catalog.clone()));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " stage        queued      batches     jobs        last        rate",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for stage in &app.ingest.stages {
        lines.push(Line::from(format!(
            " {:<12} {:<11} {:<11} {:<11} {:<11} {} {}",
            stage.name,
            stage.queued,
            stage.batches,
            stage.jobs,
            stage.last,
            stage.rate,
            stage.spark
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        " {}    q quits    1-4 jump    j/k views    enter opens",
        control_label(paused)
    )));
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().title(" ingestion ").borders(Borders::ALL))
}

fn body(app: &App, width: u16) -> Table<'static> {
    let (headers, rows, cursor) = match app.view {
        1 => survey_table(app),
        2 => peer_table(app),
        _ => (
            headers_for(app.view),
            app.rows
                .iter()
                .map(|row| {
                    Row::new(
                        row.cells
                            .iter()
                            .cloned()
                            .map(Cell::from)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
            app.cursor,
        ),
    };
    let selected = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            if index == cursor && app.focus != Focus::Nav {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    let count = headers.len().max(1);
    let each = (width.saturating_sub(4) as usize / count).clamp(8, 28) as u16;
    let widths = vec![Constraint::Length(each); count];
    let header = Row::new(
        headers
            .iter()
            .map(|name| Cell::from(*name))
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD));
    let title = if app.view == 1 {
        format!(" survey {} ", app.survey.note)
    } else {
        format!(" {} ", VIEWS[app.view])
    };
    Table::new(selected, widths)
        .header(header)
        .block(Block::default().title(title).borders(Borders::ALL))
        .column_spacing(1)
}

fn survey_table(app: &App) -> (Vec<&'static str>, Vec<Row<'static>>, usize) {
    let headers = vec![
        "name",
        "class",
        "retained",
        "value in",
        "value out",
        "count",
        "gas",
    ];
    let rows = app
        .survey
        .items
        .iter()
        .map(|item| {
            Row::new(vec![
                Cell::from(item.name.clone()),
                Cell::from(item.class.clone()),
                Cell::from(item.retained.clone()),
                Cell::from(item.value_in.clone()),
                Cell::from(item.value_out.clone()),
                Cell::from(item.count.clone()),
                Cell::from(item.gas.clone()),
            ])
        })
        .collect();
    (headers, rows, app.survey.cursor)
}

fn peer_table(app: &App) -> (Vec<&'static str>, Vec<Row<'static>>, usize) {
    let mut rows = Vec::new();
    if app.peer_down {
        rows.push(Row::new(vec![Cell::from(
            "peer service is down".to_string(),
        )]));
    }
    for line in &app.live_peers {
        rows.push(Row::new(vec![Cell::from(line.clone())]));
    }
    let live = rows.len();
    rows.extend(app.rows.iter().map(|row| {
        Row::new(
            row.cells
                .iter()
                .cloned()
                .map(Cell::from)
                .collect::<Vec<_>>(),
        )
    }));
    (
        vec![
            "peer", "address", "ports", "outcome", "ok", "fail", "too many", "served",
        ],
        rows,
        app.cursor.saturating_add(live),
    )
}

fn headers_for(view: usize) -> Vec<&'static str> {
    match view {
        3 => vec![
            "stage", "kind", "id", "attempt", "leased", "waiting", "error",
        ],
        4 => vec!["kind", "id", "tokens", "fee", "block"],
        5 => vec!["in", "out", "kinds", "width", "pieces"],
        6 => vec!["symbol", "address", "decimals", "wraps"],
        7 => vec!["name", "proxy", "decimals", "value", "time", "history"],
        8 => vec![
            "token",
            "numeraire",
            "size",
            "bought",
            "sold",
            "hops",
            "block",
        ],
        _ => vec!["row"],
    }
}

fn detail(app: &App) -> Paragraph<'static> {
    let text = if app.view == 1 {
        app.survey
            .items
            .get(app.survey.cursor)
            .map(|item| {
                if item.text.is_empty() {
                    format!("{}\n{}\nnot measured", item.name, item.class)
                } else {
                    item.text.clone()
                }
            })
            .unwrap_or_default()
    } else if app.detail.is_empty() {
        "reading".into()
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
        .block(
            Block::default()
                .title(" detail    esc back    j/k scroll ")
                .borders(Borders::ALL),
        )
}

fn footer(app: &App) -> Paragraph<'static> {
    let filter = if app.applied.is_empty() {
        String::new()
    } else {
        format!("  filter {}", app.applied)
    };
    let editing = if app.focus == Focus::Filter {
        format!("  /{}", app.filter)
    } else {
        String::new()
    };
    let sightings = app
        .sightings
        .map(|count| format!("  sightings {count}"))
        .unwrap_or_default();
    let keys = match app.view {
        1 => "r item    R survey    esc stops",
        2 => "enter summary",
        3 => "s queue/stage    enter job",
        4 => "s kind    enter pool",
        5 | 6 => "/ filter    enter detail",
        _ => "enter detail",
    };
    let notice = if app.notice.is_empty() {
        String::new()
    } else {
        format!("  {}", app.notice)
    };
    Paragraph::new(format!("{keys}{filter}{editing}{sightings}{notice}"))
        .block(Block::default().borders(Borders::ALL))
}
