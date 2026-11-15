//! Read conduits, replace the stored spots, and wait.

use store_model::Spot;

use crate::price::{mainnet_numeraires, price, CustodialWrap, Edge};

/// Read conduits, replace the stored spots, and wait. One failed pass is logged.
///
/// `gather` reads the open venue catalog. This task does not open it and does
/// not open the feed file. `wrap` is the custodial wrap from the seed file.
/// The caller opens the spot file.
pub async fn run<F>(
    book: store::service::Spots,
    mut stop: tokio::sync::watch::Receiver<bool>,
    wrap: Option<CustodialWrap>,
    settings: config::Spots,
    gather: F,
) -> eyre::Result<()>
where
    F: Fn() -> eyre::Result<Vec<Edge>> + Send + Sync + 'static,
{
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
