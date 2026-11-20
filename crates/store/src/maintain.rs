//! Offline catalog maintenance.
//!
//! The live process holds each file. This opens one existing file at a time,
//! prunes receipt blocks outside the archival window, and compacts every
//! database that is already there. A missing file is left missing.

use std::path::Path;

use eyre::WrapErr;

use crate::service::Corpus;
use crate::storage::Database;

/// One database this pass compacted.
#[derive(Debug)]
pub struct Compacted {
    pub name: String,
    pub compacted: bool,
}

/// Blocks removed from `corpus.redb`, and the files this pass compacted.
#[derive(Debug)]
pub struct Maintenance {
    pub removed_blocks: u64,
    pub files: Vec<Compacted>,
}

/// Prune corpus blocks outside `archival` and compact each existing database.
///
/// `now` is a unix timestamp. A file the live process still has open fails
/// the pass before any file is compacted.
pub fn maintain(
    dir: &Path,
    caches: &config::Store,
    archival: &config::Archival,
    now: u64,
) -> eyre::Result<Maintenance> {
    let files = catalog_files(caches);
    for (name, cache) in files {
        let path = dir.join(name);
        if path.exists() {
            probe(&path, cache)?;
        }
    }
    let mut report = Maintenance {
        removed_blocks: 0,
        files: Vec::new(),
    };
    for (name, cache) in files {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let compacted = if name == "corpus.redb" {
            let mut corpus = Corpus::create_with(&path, cache).map_err(|err| held(err, &path))?;
            report.removed_blocks = corpus.prune_blocks(archival, now)?;
            corpus.compact().wrap_err("compact corpus")?
        } else {
            let mut db = Database::open(&path, cache).map_err(|err| held(err, &path))?;
            db.compact().wrap_err_with(|| format!("compact {name}"))?
        };
        report.files.push(Compacted {
            name: name.to_string(),
            compacted,
        });
    }
    Ok(report)
}

fn catalog_files(caches: &config::Store) -> [(&'static str, usize); 9] {
    [
        ("corpus.redb", caches.corpus),
        ("headers.redb", caches.headers),
        ("discovery.redb", caches.discovery),
        ("entities.redb", caches.entities),
        ("work.redb", caches.work),
        ("feeds.redb", caches.feeds),
        ("spots.redb", caches.spots),
        ("peers.redb", caches.peers),
        ("univ3-pools.redb", caches.pools),
    ]
}

fn probe(path: &Path, cache: usize) -> eyre::Result<()> {
    match Database::open(path, cache) {
        Ok(db) => {
            drop(db);
            Ok(())
        }
        Err(err) => Err(held(err, path)),
    }
}

fn held(err: eyre::Report, path: &Path) -> eyre::Report {
    let locked = err.chain().any(|cause| {
        cause
            .downcast_ref::<redb::DatabaseError>()
            .is_some_and(|inner| matches!(inner, redb::DatabaseError::DatabaseAlreadyOpen))
            || cause.to_string().contains("Database already open")
    });
    if locked {
        eyre::eyre!("stop ratatoskr before maintenance: {}", path.display())
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;
    use std::time::{SystemTime, UNIX_EPOCH};
    use store_model::BlockRecord;

    fn temp_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-maintain-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn caches() -> config::Store {
        let byte = 1024 * 1024;
        config::Store {
            headers: byte,
            corpus: byte,
            discovery: byte,
            entities: byte,
            work: byte,
            read: byte,
            peers: byte,
            feeds: byte,
            spots: byte,
            pools: byte,
            header_batch: 64,
            block_page: byte,
        }
    }

    fn record(number: u64, timestamp: u64) -> BlockRecord {
        BlockRecord {
            number,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            parent_hash: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            timestamp,
            receipts_root: alloy_primitives::B256::ZERO,
            transactions_root: alloy_primitives::B256::ZERO,
            receipts_zstd: vec![1, 2, 3],
        }
    }

    #[test]
    fn maintain_prunes_old_blocks_and_compacts_existing_files() {
        let dir = temp_dir();
        let now = 1_700_000_000_u64;
        let window = 100_u64;
        let corpus = Corpus::create_with(&dir.join("corpus.redb"), caches().corpus).unwrap();
        corpus
            .put_blocks(&[record(1, now - window - 1), record(2, now - window)])
            .unwrap();
        corpus.write_hole(3).unwrap();
        drop(corpus);
        Database::create(&dir.join("headers.redb"), caches().headers).unwrap();

        let report = maintain(
            &dir,
            &caches(),
            &config::Archival {
                blocks_secs: window,
            },
            now,
        )
        .unwrap();
        assert_eq!(report.removed_blocks, 2);
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.files[0].name, "corpus.redb");
        assert_eq!(report.files[1].name, "headers.redb");
        assert!(!dir.join("discovery.redb").exists());
        assert!(!dir.join("entities.redb").exists());

        let corpus = Corpus::create_with(&dir.join("corpus.redb"), caches().corpus).unwrap();
        assert!(corpus.read_block(1).unwrap().is_none());
        assert_eq!(
            corpus.read_block(2).unwrap().unwrap().timestamp,
            now - window
        );
        assert_eq!(corpus.meta("holes").unwrap(), Some(0));
        drop(corpus);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maintain_refuses_a_file_the_live_process_holds() {
        let dir = temp_dir();
        let path = dir.join("corpus.redb");
        let corpus = Corpus::create_with(&path, caches().corpus).unwrap();
        corpus.put_blocks(&[record(1, 50)]).unwrap();
        drop(corpus);
        let held = Database::open(&path, caches().corpus).unwrap();
        let err = maintain(&dir, &caches(), &config::Archival::default(), 0).unwrap_err();
        assert!(
            err.to_string()
                .contains("stop ratatoskr before maintenance"),
            "{err:#}"
        );
        drop(held);
        let corpus = Corpus::create_with(&path, caches().corpus).unwrap();
        assert_eq!(corpus.read_block(1).unwrap().unwrap().number, 1);
        drop(corpus);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
