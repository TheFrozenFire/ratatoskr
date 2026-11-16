//! Read conduits, replace the stored spots, and wait.

use store_model::Spot;

use crate::price::{mainnet_numeraires, price, CustodialWrap, Edge};

/// Read conduits, replace the stored spots, and wait. One failed pass is logged.
///
/// `gather` reads the open venue catalog. This task does not open it and does
/// not open the feed file. The custodial wrap is the seed file's first pair.
/// The caller opens the spot file.
pub async fn run<F>(
    book: store::service::Spots,
    mut stop: tokio::sync::watch::Receiver<bool>,
    settings: config::Spots,
    gather: F,
) -> eyre::Result<()>
where
    F: Fn() -> eyre::Result<Vec<Edge>> + Send + Sync + 'static,
{
    let (native, wrapped) = chain::custodial_wrap();
    let wrap = Some(CustodialWrap { native, wrapped });
    let gather = std::sync::Arc::new(gather);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let gather = std::sync::Arc::clone(&gather);
        let floor = settings.hub_floor;
        let limit = settings.hub_limit;
        let found = tokio::task::spawn_blocking(move || -> eyre::Result<Vec<Spot>> {
            let edges = gather()?;
            Ok(price(&edges, floor, limit, &mainnet_numeraires(), wrap))
        })
        .await
        .map_err(|err| eyre::eyre!("spot pass: {err}"))?;
        match found {
            Ok(spots) => {
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
