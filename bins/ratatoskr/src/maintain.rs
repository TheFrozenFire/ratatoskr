//! Prune and compact the catalog while the live process is stopped.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use eyre::WrapErr;

pub(crate) async fn run(
    data_dir: &Path,
    caches: &config::Store,
    archival: &config::Archival,
) -> eyre::Result<()> {
    let data_dir = data_dir.to_path_buf();
    let caches = caches.clone();
    let archival = *archival;
    let report = tokio::task::spawn_blocking(move || {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        store::maintain(&data_dir, &caches, &archival, now)
    })
    .await
    .wrap_err("maintenance task")??;
    println!("removed {} blocks", report.removed_blocks);
    for file in &report.files {
        if file.compacted {
            println!("{} compacted", file.name);
        } else {
            println!("{} already compact", file.name);
        }
    }
    Ok(())
}
