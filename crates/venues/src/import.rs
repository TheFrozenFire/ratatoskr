//! Fold the local receipt corpus into the venue catalog.
//!
//! The corpus is a partial slice. It does not replace a lens or a log walk.

use eyre::eyre;

use crate::catalog::Put;
use crate::decode::rows_from_log;
use crate::EntityStore;
use corpus::logs_in_block;
use model::decode_block_record;

pub async fn import_corpus<S>(store: &S) -> eyre::Result<u64>
where
    S: EntityStore + Clone + 'static,
{
    let cursor = store.cursor("corpus").await?.unwrap_or(0);
    let mut pending = Vec::new();
    let mut next = cursor;
    let mut wrote = 0_u64;
    let writer = store.clone();
    store
        .for_each_block(&mut |number, bytes| {
            if number < cursor {
                return Ok(());
            }
            next = number.saturating_add(1);
            if !bytes.is_empty() {
                let (record, _) = decode_block_record(bytes).map_err(|err| eyre!(err))?;
                for log in logs_in_block(&record)? {
                    pending.extend(rows_from_log(&log));
                }
            }
            if number % 2_000 == 0 {
                let batch = std::mem::take(&mut pending);
                let writer = writer.clone();
                let at = next;
                // The scan callback cannot await. A runtime `block_on` here
                // deadlocks the caller, so the commit runs on its own runtime.
                wrote += std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| eyre!("commit runtime: {err}"))?;
                    runtime.block_on(writer.commit_chunk("corpus", at, &batch, Put::Merge))
                })
                .join()
                .unwrap_or_else(|_| Err(eyre!("corpus commit panicked")))?;
                tracing::info!(block = number, "corpus");
            }
            Ok(())
        })
        .await?;
    wrote += store
        .commit_chunk("corpus", next, &pending, Put::Merge)
        .await?;
    Ok(wrote)
}
