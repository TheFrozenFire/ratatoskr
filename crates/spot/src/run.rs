//! Read conduits, replace the stored spots, and wait.

use std::collections::HashMap;

use alloy_primitives::Address;
use store_model::Spot;

use crate::price::{mainnet_numeraires, price, CustodialWrap, Edge};

/// Read conduits, replace the stored spots, and wait. One failed pass is logged.
///
/// The first pass reads every stored conduit. Later passes apply the conduit
/// log and skip pricing when nothing was written. This task does not open the
/// feed file. The custodial wrap is the seed file's first pair. The caller
/// opens the spot file.
pub async fn run(
    book: store::service::Spots,
    store: store::LocalStore,
    mut stop: tokio::sync::watch::Receiver<bool>,
    settings: config::Spots,
) -> eyre::Result<()> {
    let (native, wrapped) = chain::custodial_wrap();
    let wrap = Some(CustodialWrap { native, wrapped });
    let mut held = Held::default();
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let floor = settings.hub_floor;
        let limit = settings.hub_limit;
        let store = store.clone();
        let outcome =
            tokio::task::spawn_blocking(move || -> (eyre::Result<Option<Vec<Spot>>>, Held) {
                let read = refresh(&store, &mut held).map(|changed| {
                    changed.then(|| {
                        price(
                            held.edges.values(),
                            floor,
                            limit,
                            &mainnet_numeraires(),
                            wrap,
                        )
                    })
                });
                (read, held)
            })
            .await
            .map_err(|err| eyre::eyre!("spot pass: {err}"))?;
        held = outcome.1;
        let found = outcome.0;
        match found {
            Ok(None) => observability::info!(changed = 0, "spots"),
            Ok(Some(spots)) => {
                let count = spots.len() as u64;
                match book.replace(&spots) {
                    Ok(()) => {
                        observability::note_spots(count);
                        observability::info!(spots = count, "spots");
                    }
                    Err(err) => observability::error!(%err, "spots"),
                }
            }
            Err(err) => observability::error!(%err, "spots"),
        }
        if *stop.borrow() {
            return Ok(());
        }
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            _ = tokio::time::sleep(settings.pass()) => {}
        }
    }
}

#[derive(Default)]
struct Held {
    edges: HashMap<(Address, Address), Edge>,
    /// Next log sequence already applied. Empty until the first full read.
    next: Option<u64>,
}

/// `false` means the stored conduits are unchanged since the last pass.
fn refresh(store: &store::LocalStore, held: &mut Held) -> eyre::Result<bool> {
    let tip = store.conduit_log_tip()?;
    if tip.overflow || held.next.is_none() {
        if tip.overflow {
            store.clear_conduit_log()?;
        }
        let tip = store.conduit_log_tip()?;
        held.edges.clear();
        store.visit_conduit_blocks(|token_in, token_out, block, segments| {
            held.edges.insert(
                (token_in, token_out),
                Edge {
                    token_in,
                    token_out,
                    block: Some(block),
                    segments: segments.to_vec(),
                },
            );
            Ok(())
        })?;
        let (notes, after) = store.conduit_log_since(tip.next)?;
        let changed = notes.len();
        apply(held, &notes);
        store.trim_conduit_log(after.next)?;
        held.next = Some(after.next);
        if after.overflow {
            held.next = None;
            return refresh(store, held);
        }
        observability::info!(
            conduits = held.edges.len(),
            changed,
            full = true,
            "spots read"
        );
        return Ok(true);
    }
    let cursor = held
        .next
        .expect("a full conduit read stores the log cursor");
    let (notes, after) = store.conduit_log_since(cursor)?;
    if after.overflow {
        held.next = None;
        return refresh(store, held);
    }
    if notes.is_empty() && after.next == cursor {
        return Ok(false);
    }
    let changed = notes.len();
    apply(held, &notes);
    store.trim_conduit_log(after.next)?;
    held.next = Some(after.next);
    observability::info!(
        conduits = held.edges.len(),
        changed,
        full = false,
        "spots read"
    );
    Ok(true)
}

fn apply(held: &mut Held, notes: &[store::service::ConduitNote]) {
    for note in notes {
        let key = (note.token_in, note.token_out);
        match &note.block {
            Some(block) => {
                held.edges.insert(
                    key,
                    Edge {
                        token_in: note.token_in,
                        token_out: note.token_out,
                        block: Some(*block),
                        segments: note.segments.clone(),
                    },
                );
            }
            None => {
                held.edges.remove(&key);
            }
        }
    }
}

/// Stored pair conduits as spot edges. The block is the one the merge used.
pub fn conduit_edges(store: &store::LocalStore) -> eyre::Result<Vec<Edge>> {
    let mut edges = Vec::new();
    store.visit_conduit_blocks(|token_in, token_out, block, segments| {
        edges.push(Edge {
            token_in,
            token_out,
            block: Some(block),
            segments: segments.to_vec(),
        });
        Ok(())
    })?;
    Ok(edges)
}
