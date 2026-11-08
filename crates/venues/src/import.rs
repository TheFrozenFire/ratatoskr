//! Fold the local receipt corpus into the venue catalog.
//!
//! The corpus is a partial slice. It does not replace a lens or a log walk.

use eyre::eyre;
use std::path::Path;

use crate::catalog::Put;
use crate::decode::rows_from_log;
use crate::Catalog;
use corpus::{logs_in_block, Corpus};
use model::decode_block_record;

pub fn import_corpus(catalog: &Catalog, path: &Path) -> eyre::Result<u64> {
    let corpus = Corpus::open(path)?;
    let cursor = catalog.cursor("corpus")?.unwrap_or(0);
    let mut pending = Vec::new();
    let mut next = cursor;
    let mut wrote = 0_u64;
    corpus.for_each_block(|number, bytes| {
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
            wrote += catalog.commit_chunk("corpus", next, &pending, Put::Merge)?;
            pending.clear();
            tracing::info!(block = number, "corpus");
        }
        Ok(())
    })?;
    wrote += catalog.commit_chunk("corpus", next, &pending, Put::Merge)?;
    Ok(wrote)
}
