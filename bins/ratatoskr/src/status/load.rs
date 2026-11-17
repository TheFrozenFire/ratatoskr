//! Read-only pages. Nothing here writes a catalog file.

use alloy_primitives::{Address, B256};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use store::service::Shelf;
use store_model::kind_name;

use super::ingest::Local;
use super::model::{App, Row, RowKey, PAGE};

pub(crate) struct Names {
    map: HashMap<Address, String>,
    loaded: bool,
}

impl Names {
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
            loaded: false,
        }
    }

    fn ensure(&mut self, shelf: &Shelf) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(entities) = shelf.entities.as_ref() else {
            return;
        };
        if let Ok(tokens) = entities.tokens() {
            for token in tokens {
                self.map.insert(token.address, token.symbol);
            }
        }
    }

    fn label(&self, address: Address) -> String {
        self.map
            .get(&address)
            .filter(|symbol| !symbol.is_empty())
            .cloned()
            .unwrap_or_else(|| short_addr(address))
    }
}

pub(crate) struct Browser {
    gen: u64,
    stack: Vec<Vec<u8>>,
    pub(crate) after: Vec<u8>,
}

impl Browser {
    pub(crate) fn new() -> Self {
        Self {
            gen: 0,
            stack: Vec::new(),
            after: Vec::new(),
        }
    }

    pub(crate) fn turn(&mut self, app: &mut App) {
        if self.gen != app.reload {
            self.gen = app.reload;
            self.stack.clear();
            self.after.clear();
            app.cursor = 0;
        }
        match app.nudge {
            super::model::Nudge::Next => {
                if let Some(last) = app.rows.last() {
                    if !last.resume.is_empty() {
                        self.stack.push(std::mem::take(&mut self.after));
                        self.after = last.resume.clone();
                        app.cursor = 0;
                    }
                }
            }
            super::model::Nudge::Prev => {
                self.after = self.stack.pop().unwrap_or_default();
                app.cursor = 0;
            }
            super::model::Nudge::Stay => {}
        }
        app.nudge = super::model::Nudge::Stay;
    }
}

pub(crate) fn local_of(shelf: &Shelf, exporter_up: bool) -> Local {
    let tip = shelf.tip();
    let mut local = Local {
        header_number: tip.as_ref().map(|header| header.number),
        header_timestamp: tip.as_ref().map(|header| header.timestamp),
        header_hash: tip
            .as_ref()
            .map(|header| header.hash.to_string())
            .unwrap_or_default(),
        corpus_bytes: shelf.corpus_bytes(),
        corpus_holes: shelf.corpus_holes(),
        cursors: shelf.cursors(),
        ..Local::default()
    };
    if exporter_up {
        return local;
    }
    local.token_count = shelf
        .entities
        .as_ref()
        .and_then(|entities| entities.token_count().ok());
    local.feed_count = shelf.feeds.as_ref().and_then(|feeds| feeds.count().ok());
    local.spot_count = shelf.spots.as_ref().and_then(|spots| spots.count().ok());
    if let Some(work) = shelf.work.as_ref() {
        for (index, stage) in [1_u8, 2, 3, 4, 6].into_iter().enumerate() {
            local.queued[index] = work.count_stage(stage).ok();
        }
    }
    local
}

pub(crate) fn refresh(app: &mut App, shelf: &Shelf, names: &mut Names, after: &[u8]) {
    names.ensure(shelf);
    let now = unix_now();
    let loaded = match app.view {
        2 => peers(shelf, after),
        3 => work(shelf, app.work_conduit, app.work_stage, after, now, names),
        4 => pools(shelf, app.pool_kind, after, names),
        5 => conduits(shelf, after, &app.applied, names),
        6 => tokens(shelf, after, &app.applied),
        7 => feeds(shelf, after),
        8 => spots(shelf, after, names),
        _ => Ok(Vec::new()),
    };
    match loaded {
        Ok(rows) => {
            app.page_full = rows.len() == PAGE;
            app.rows = rows;
            if app.cursor >= app.rows.len() {
                app.cursor = app.rows.len().saturating_sub(1);
            }
            app.notice.clear();
        }
        Err(err) => {
            app.rows.clear();
            app.page_full = false;
            app.notice = err;
        }
    }
}

pub(crate) fn fill_detail(app: &mut App, shelf: &Shelf, names: &mut Names) {
    if !app.detail_pending {
        return;
    }
    names.ensure(shelf);
    app.detail_pending = false;
    let Some(row) = app.rows.get(app.cursor) else {
        return;
    };
    if !row.detail.is_empty() {
        app.detail = row.detail.clone();
        return;
    }
    let key = row.key.clone();
    app.detail = match key {
        RowKey::Pool { kind, id } => pool_detail(shelf, kind, id),
        RowKey::Conduit {
            token_in,
            token_out,
        } => conduit_detail(shelf, token_in, token_out),
        RowKey::Token(address) => token_detail(shelf, address, names),
        RowKey::Feed(proxy) => feed_detail(shelf, proxy),
        RowKey::Spot { token, numeraire } => spot_detail(shelf, token, numeraire, names),
        _ => String::new(),
    };
}

pub(crate) fn live_peers(socket: &Path) -> Result<Vec<String>, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(400)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(400)))
        .ok();
    stream
        .write_all(b"{\"op\":\"status\"}\n")
        .map_err(|err| err.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| err.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|err| err.to_string())?;
    let Some(peers) = value["peers"].as_array() else {
        let err = value["error"].as_str().unwrap_or("peer service is down");
        return Err(err.to_string());
    };
    Ok(peers
        .iter()
        .map(|peer| {
            format!(
                "{}  eth {}  head {}  earliest {}",
                peer["peer"].as_str().unwrap_or(""),
                json_num(&peer["eth"]),
                json_num(&peer["head"]),
                json_num(&peer["earliest"]),
            )
        })
        .collect())
}

fn peers(shelf: &Shelf, after: &[u8]) -> Result<Vec<Row>, String> {
    let peers = shelf.peers.as_ref().ok_or("peers file is closed")?;
    let after = if after.is_empty() { None } else { Some(after) };
    let page = peers.page(after, PAGE).map_err(|err| err.to_string())?;
    Ok(page
        .into_iter()
        .map(|row| {
            let summary = &row.summary;
            let id = row.id.to_string();
            let detail = format!(
                "{id}\n{} tcp {} udp {}\neth {}\n{}\nsuccess {}  fail {}  too many {}\nserved {} .. {}\nearliest {}  latest {}",
                summary.address,
                summary.tcp_port,
                summary.udp_port,
                summary.eth,
                outcome(summary.last_outcome),
                summary.success,
                summary.fail,
                summary.too_many,
                opt_num(summary.lowest_served),
                opt_num(summary.highest_served),
                opt_num(summary.last_earliest),
                opt_num(summary.last_latest),
            );
            Row {
                cells: vec![
                    short_text(&id),
                    summary.address.to_string(),
                    format!("{}/{}", summary.tcp_port, summary.udp_port),
                    outcome(summary.last_outcome).into(),
                    summary.success.to_string(),
                    summary.fail.to_string(),
                    summary.too_many.to_string(),
                    opt_num(summary.highest_served),
                ],
                resume: row.id.as_slice().to_vec(),
                detail,
                key: RowKey::Peer(0),
            }
        })
        .collect())
}

fn work(
    shelf: &Shelf,
    conduit: bool,
    stage: Option<u8>,
    after: &[u8],
    now: u64,
    names: &Names,
) -> Result<Vec<Row>, String> {
    let work = shelf.work.as_ref().ok_or("work file is closed")?;
    let after = if after.is_empty() { None } else { Some(after) };
    if conduit {
        let page = work
            .page_conduit_jobs(after, PAGE)
            .map_err(|err| err.to_string())?;
        return Ok(page
            .into_iter()
            .map(|job| {
                let resume = pair_bytes(job.token_in, job.token_out);
                let detail = format!(
                    "{} -> {}\nattempt {}\nleased {}\nwaiting {}\nrebuild {}\n{}",
                    names.label(job.token_in),
                    names.label(job.token_out),
                    job.attempt,
                    ahead(job.lease_until, now),
                    ahead(job.not_before, now),
                    job.rebuild,
                    job.error
                );
                Row {
                    cells: vec![
                        "conduit".into(),
                        names.label(job.token_in),
                        names.label(job.token_out),
                        job.attempt.to_string(),
                        ahead(job.lease_until, now).into(),
                        ahead(job.not_before, now).into(),
                        job.error.clone(),
                    ],
                    key: RowKey::ConduitWork(0),
                    detail,
                    resume,
                }
            })
            .collect());
    }
    let page = work
        .page_jobs(stage, after, PAGE)
        .map_err(|err| err.to_string())?;
    Ok(page
        .into_iter()
        .map(|job| {
            let mut resume = vec![job.stage, job.kind];
            resume.extend_from_slice(job.id.as_slice());
            let detail = format!(
                "{} {} {}\nattempt {}\ngeneration {}\nleased {}\nwaiting {}\n{}",
                stage_name(job.stage),
                kind_name(job.kind),
                job.id,
                job.attempt,
                job.generation,
                ahead(job.lease_until, now),
                ahead(job.not_before, now),
                job.error
            );
            Row {
                cells: vec![
                    stage_name(job.stage).into(),
                    kind_name(job.kind).into(),
                    short_b256(&job.id),
                    job.attempt.to_string(),
                    ahead(job.lease_until, now).into(),
                    ahead(job.not_before, now).into(),
                    job.error.clone(),
                ],
                key: RowKey::Work(0),
                detail,
                resume,
            }
        })
        .collect())
}

fn pools(shelf: &Shelf, kind: Option<u8>, after: &[u8], names: &Names) -> Result<Vec<Row>, String> {
    let entities = shelf.entities.as_ref().ok_or("entity file is closed")?;
    let mut rows = Vec::new();
    let kinds: Vec<u8> = match kind {
        Some(kind) => vec![kind],
        None => (1..=14).collect(),
    };
    let (parsed_kind, start_id) = pool_after(after);
    let start_kind = parsed_kind.or(kind.filter(|_| after.len() == 32));
    for kind in &kinds {
        if start_kind.is_some_and(|start| *kind < start) {
            continue;
        }
        let after_id = if start_kind == Some(*kind) {
            start_id
        } else {
            None
        };
        let page = entities
            .page_venues(*kind, after_id, PAGE - rows.len())
            .map_err(|err| err.to_string())?;
        for venue in page {
            let mut resume = Vec::new();
            if kinds.len() != 1 {
                resume.push(venue.kind);
            }
            resume.extend_from_slice(venue.id.as_slice());
            let tokens = venue
                .tokens
                .iter()
                .map(|token| names.label(*token))
                .collect::<Vec<_>>()
                .join(" ");
            rows.push(Row {
                cells: vec![
                    kind_name(venue.kind).into(),
                    short_b256(&venue.id),
                    tokens,
                    venue.fee.map(|fee| fee.to_string()).unwrap_or_default(),
                    venue
                        .block_number
                        .map(|block| block.to_string())
                        .unwrap_or_default(),
                ],
                key: RowKey::Pool {
                    kind: venue.kind,
                    id: venue.id,
                },
                detail: String::new(),
                resume,
            });
            if rows.len() == PAGE {
                return Ok(rows);
            }
        }
    }
    Ok(rows)
}

fn conduits(shelf: &Shelf, after: &[u8], prefix: &str, names: &Names) -> Result<Vec<Row>, String> {
    let entities = shelf.entities.as_ref().ok_or("entity file is closed")?;
    let cursor = pair_after(after);
    let scan = if prefix.is_empty() { PAGE } else { PAGE * 64 };
    let page = entities
        .page_conduits(cursor, scan)
        .map_err(|err| err.to_string())?;
    let mut rows = Vec::new();
    for row in page {
        if !prefix.is_empty() && !pair_matches(prefix, row.token_in, row.token_out, names) {
            continue;
        }
        rows.push(Row {
            cells: vec![
                names.label(row.token_in),
                names.label(row.token_out),
                kind_mask(row.meta.kinds),
                row.meta.capacity.to_string(),
                row.meta.segments.to_string(),
            ],
            resume: pair_bytes(row.token_in, row.token_out),
            key: RowKey::Conduit {
                token_in: row.token_in,
                token_out: row.token_out,
            },
            detail: String::new(),
        });
        if rows.len() == PAGE {
            break;
        }
    }
    Ok(rows)
}

fn tokens(shelf: &Shelf, after: &[u8], prefix: &str) -> Result<Vec<Row>, String> {
    let entities = shelf.entities.as_ref().ok_or("entity file is closed")?;
    let cursor = addr_after(after);
    let scan = if prefix.is_empty() { PAGE } else { PAGE * 64 };
    let page = entities
        .page_tokens(cursor, scan)
        .map_err(|err| err.to_string())?;
    let mut rows = Vec::new();
    for token in page {
        if !prefix.is_empty()
            && !token
                .symbol
                .to_ascii_lowercase()
                .starts_with(&prefix_key(prefix))
            && !address_prefix(token.address, &prefix_key(prefix))
        {
            continue;
        }
        rows.push(Row {
            cells: vec![
                token.symbol.clone(),
                token.address.to_string(),
                token
                    .decimals
                    .map(|decimals| decimals.to_string())
                    .unwrap_or_default(),
                token
                    .wraps
                    .map(|wraps| wraps.to_string())
                    .unwrap_or_default(),
            ],
            resume: token.address.as_slice().to_vec(),
            key: RowKey::Token(token.address),
            detail: String::new(),
        });
        if rows.len() == PAGE {
            break;
        }
    }
    Ok(rows)
}

fn feeds(shelf: &Shelf, after: &[u8]) -> Result<Vec<Row>, String> {
    let feeds = shelf.feeds.as_ref().ok_or("feeds file is closed")?;
    let after = if after.is_empty() { None } else { Some(after) };
    let page = feeds.page(after, PAGE).map_err(|err| err.to_string())?;
    Ok(page
        .into_iter()
        .map(|line| {
            let proxy = line.feed.row.proxy;
            let value = line
                .latest
                .as_ref()
                .map(|answer| answer.value.to_string())
                .unwrap_or_default();
            let time = line
                .latest
                .as_ref()
                .map(|answer| answer.updated_at.to_string())
                .unwrap_or_default();
            Row {
                cells: vec![
                    line.feed.row.name.clone(),
                    proxy.to_string(),
                    line.feed.row.decimals.to_string(),
                    value,
                    time,
                    if line.history {
                        "yes".into()
                    } else {
                        String::new()
                    },
                ],
                resume: proxy.as_slice().to_vec(),
                key: RowKey::Feed(proxy),
                detail: String::new(),
            }
        })
        .collect())
}

fn spots(shelf: &Shelf, after: &[u8], names: &Names) -> Result<Vec<Row>, String> {
    let spots = shelf.spots.as_ref().ok_or("spots file is closed")?;
    let after = if after.is_empty() { None } else { Some(after) };
    let page = spots.page(after, PAGE).map_err(|err| err.to_string())?;
    Ok(page
        .into_iter()
        .map(|spot| Row {
            cells: vec![
                names.label(spot.token),
                names.label(spot.numeraire),
                spot.size.to_string(),
                spot.bought.to_string(),
                spot.sold.to_string(),
                spot.hops.to_string(),
                spot.block.to_string(),
            ],
            resume: pair_bytes(spot.token, spot.numeraire),
            key: RowKey::Spot {
                token: spot.token,
                numeraire: spot.numeraire,
            },
            detail: String::new(),
        })
        .collect())
}

fn pool_detail(shelf: &Shelf, kind: u8, id: B256) -> String {
    let Some(entities) = shelf.entities.as_ref() else {
        return "entity file is closed".into();
    };
    let card = match entities.pool_card(kind, id) {
        Ok(card) => card,
        Err(err) => return err.to_string(),
    };
    let job = shelf
        .work
        .as_ref()
        .and_then(|work| work.job_for(kind, id).ok())
        .flatten();
    let mut lines = vec![
        format!("{} {}", kind_name(kind), id),
        format!("immutable {}", card.immutable),
        format!("mutable block {}", opt_num(card.block)),
        format!("generation {}", opt_num(card.generation)),
        format!("ticks {}", opt_num(card.tick_count.map(u64::from))),
        format!("truncated {}", opt_bool(card.truncated)),
        format!("next scan {}", opt_i32(card.next_scan)),
        format!("curve generation {}", opt_num(card.curve_generation)),
        format!(
            "curve version {}",
            opt_num(card.curve_version.map(u64::from))
        ),
    ];
    if let Some(job) = job {
        lines.push(format!(
            "work {} attempt {} {}",
            stage_name(job.stage),
            job.attempt,
            job.error
        ));
    } else {
        lines.push("work none".into());
    }
    lines.join("\n")
}

fn conduit_detail(shelf: &Shelf, token_in: Address, token_out: Address) -> String {
    let Some(entities) = shelf.entities.as_ref() else {
        return "entity file is closed".into();
    };
    match entities.conduit(token_in, token_out) {
        Ok(Some(stored)) => {
            let mut lines = vec![format!(
                "block {}  generation {}  members {}",
                stored.block, stored.generation, stored.members
            )];
            for segment in &stored.segments {
                lines.push(format!(
                    "width {}  output {}",
                    segment.width, segment.output
                ));
            }
            if stored.segments.is_empty() {
                lines.push("no chords".into());
            }
            lines.join("\n")
        }
        Ok(None) => "conduit is gone".into(),
        Err(err) => err.to_string(),
    }
}

fn token_detail(shelf: &Shelf, address: Address, names: &Names) -> String {
    let Some(entities) = shelf.entities.as_ref() else {
        return "entity file is closed".into();
    };
    let record = match entities.token(address) {
        Ok(Some(record)) => record,
        Ok(None) => return "token is gone".into(),
        Err(err) => return err.to_string(),
    };
    let spots = shelf
        .spots
        .as_ref()
        .and_then(|spots| spots.for_token(address).ok())
        .unwrap_or_default();
    let named = entities
        .conduits_naming(address)
        .map(|count| count.to_string())
        .unwrap_or_else(|err| err.to_string());
    let mut lines = vec![
        format!("{} {}", record.symbol, record.address),
        format!(
            "decimals {}",
            record
                .decimals
                .map(|decimals| decimals.to_string())
                .unwrap_or_else(|| "—".into())
        ),
        format!(
            "wraps {}",
            record
                .wraps
                .map(|wraps| names.label(wraps))
                .unwrap_or_else(|| "—".into())
        ),
        format!("conduits {named}"),
    ];
    for spot in spots {
        lines.push(format!(
            "spot {} size {} hops {} block {}",
            names.label(spot.numeraire),
            spot.size,
            spot.hops,
            spot.block
        ));
    }
    lines.join("\n")
}

fn feed_detail(shelf: &Shelf, proxy: Address) -> String {
    let Some(feeds) = shelf.feeds.as_ref() else {
        return "feeds file is closed".into();
    };
    match feeds.detail(proxy) {
        Ok(Some(detail)) => {
            let mut lines = vec![
                format!("{} {}", detail.feed.row.name, detail.feed.row.proxy),
                format!(
                    "{} / {}  decimals {}",
                    detail.feed.row.base_asset,
                    detail.feed.row.quote_asset,
                    detail.feed.row.decimals
                ),
                detail.feed.row.path.clone(),
            ];
            for phase in &detail.phases {
                lines.push(format!(
                    "phase {}  {}  min {}  max {}",
                    phase.phase_id,
                    phase.aggregator,
                    opt_signed(phase.min_answer.as_ref()),
                    opt_signed(phase.max_answer.as_ref())
                ));
            }
            for answer in &detail.answers {
                lines.push(format!(
                    "round {}  {}  block {}  at {}",
                    answer.round_id, answer.value, answer.block, answer.updated_at
                ));
            }
            lines.join("\n")
        }
        Ok(None) => "feed is gone".into(),
        Err(err) => err.to_string(),
    }
}

fn spot_detail(shelf: &Shelf, token: Address, numeraire: Address, names: &Names) -> String {
    let Some(spots) = shelf.spots.as_ref() else {
        return "spots file is closed".into();
    };
    let page = spots.for_token(token).unwrap_or_default();
    let Some(spot) = page.into_iter().find(|spot| spot.numeraire == numeraire) else {
        return "spot is gone".into();
    };
    format!(
        "{} / {}\nsize {}\nbought {}\nsold {}\nhops {}\nblock {}",
        names.label(spot.token),
        names.label(spot.numeraire),
        spot.size,
        spot.bought,
        spot.sold,
        spot.hops,
        spot.block
    )
}

fn pool_after(after: &[u8]) -> (Option<u8>, Option<B256>) {
    match after.len() {
        33 => (Some(after[0]), Some(B256::from_slice(&after[1..]))),
        32 => (None, Some(B256::from_slice(after))),
        _ => (None, None),
    }
}

fn pair_after(after: &[u8]) -> Option<(Address, Address)> {
    if after.len() != 40 {
        return None;
    }
    Some((
        Address::from_slice(&after[..20]),
        Address::from_slice(&after[20..]),
    ))
}

fn addr_after(after: &[u8]) -> Option<Address> {
    if after.len() != 20 {
        None
    } else {
        Some(Address::from_slice(after))
    }
}

fn pair_bytes(left: Address, right: Address) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(left.as_slice());
    out.extend_from_slice(right.as_slice());
    out
}

fn pair_matches(prefix: &str, token_in: Address, token_out: Address, names: &Names) -> bool {
    let prefix = prefix_key(prefix);
    let hit = |address: Address| {
        address_prefix(address, &prefix)
            || names
                .map
                .get(&address)
                .is_some_and(|symbol| symbol.to_ascii_lowercase().starts_with(&prefix))
    };
    hit(token_in) || hit(token_out)
}

fn address_prefix(address: Address, prefix: &str) -> bool {
    address
        .to_string()
        .trim_start_matches("0x")
        .to_ascii_lowercase()
        .starts_with(prefix)
}

fn prefix_key(prefix: &str) -> String {
    prefix.trim().trim_start_matches("0x").to_ascii_lowercase()
}

fn kind_mask(mask: u32) -> String {
    let mut names = Vec::new();
    for kind in 1..=14 {
        if mask & (1 << (kind - 1)) != 0 {
            names.push(kind_name(kind));
        }
    }
    if names.is_empty() {
        "—".into()
    } else {
        names.join(",")
    }
}

fn stage_name(stage: u8) -> &'static str {
    match stage {
        1 => "immutable",
        2 => "mutable",
        3 => "ticks",
        4 => "curve",
        6 => "conduit",
        _ => "other",
    }
}

fn outcome(code: u8) -> &'static str {
    match code {
        1 => "status",
        2 => "served",
        3 => "empty",
        4 => "dropped",
        5 => "dial failed",
        6 => "too many",
        _ => "other",
    }
}

fn ahead(until: u64, now: u64) -> &'static str {
    if until > now {
        "yes"
    } else {
        "no"
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn short_addr(address: Address) -> String {
    short_text(&address.to_string())
}

fn short_b256(id: &B256) -> String {
    short_text(&id.to_string())
}

fn short_text(text: &str) -> String {
    if text.len() <= 14 {
        return text.to_string();
    }
    format!("{}…{}", &text[..6], &text[text.len() - 4..])
}

fn opt_num(value: Option<impl ToString>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".into())
}

fn opt_bool(value: Option<bool>) -> String {
    match value {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "—".into(),
    }
}

fn opt_i32(value: Option<i32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".into())
}

fn opt_signed(value: Option<&impl ToString>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".into())
}

fn json_num(value: &serde_json::Value) -> String {
    value
        .as_u64()
        .map(|number| number.to_string())
        .unwrap_or_else(|| "—".into())
}
