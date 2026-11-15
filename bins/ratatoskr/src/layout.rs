//! Paths of the files this process owns, and which combination may open them.

use crate::cli::{Pipelines, Services};
use std::path::{Path, PathBuf};
use store::LocalStore;

pub(crate) struct Paths {
    pub(crate) peers: PathBuf,
    pub(crate) headers: PathBuf,
    pub(crate) corpus: PathBuf,
    pub(crate) venues: PathBuf,
    pub(crate) peer_socket: PathBuf,
    pub(crate) feeds: PathBuf,
    pub(crate) spots: PathBuf,
}

pub(crate) struct Owned {
    pub(crate) peer: Option<peer::OpenedPeer>,
    pub(crate) store: Option<LocalStore>,
}

pub(crate) fn check_combination(services: &Services, pipelines: &Pipelines) -> eyre::Result<()> {
    if pipelines.headers && !(services.db && services.peer) {
        return Err(eyre::eyre!("headers needs both db and peer"));
    }
    if pipelines.venues && !services.db {
        return Err(eyre::eyre!("venues needs db"));
    }
    if pipelines.state && !services.db {
        return Err(eyre::eyre!("state needs db"));
    }
    if pipelines.spots && !services.db {
        return Err(eyre::eyre!("spots needs db"));
    }
    Ok(())
}

/// Open the selected files. `peers.redb` is first. A bad combination opens nothing.
pub(crate) fn open_owned(
    services: &Services,
    pipelines: &Pipelines,
    paths: &Paths,
    caches: &config::Store,
) -> eyre::Result<Owned> {
    check_combination(services, pipelines)?;
    let peer_owner = if services.peer {
        Some(peer::open_with(&paths.peers, caches.peers)?)
    } else {
        None
    };
    let catalog = services.db
        && (pipelines.headers || pipelines.venues || pipelines.state || pipelines.spots);
    let store = if catalog {
        match LocalStore::open_with(&paths.headers, &paths.corpus, &paths.venues, caches) {
            Ok(store) => Some(store),
            Err(err) => {
                drop(peer_owner);
                return Err(err);
            }
        }
    } else {
        None
    };
    Ok(Owned {
        peer: peer_owner,
        store,
    })
}

pub(crate) fn paths_from(dir: &Path, socket: &Option<String>) -> eyre::Result<Paths> {
    Ok(Paths {
        peers: dir.join("peers.redb"),
        headers: dir.join("headers.redb"),
        corpus: dir.join("corpus.redb"),
        venues: dir.join("entities.redb"),
        peer_socket: config::peer_socket(dir, socket)?,
        feeds: dir.join("feeds.redb"),
        spots: dir.join("spots.redb"),
    })
}

#[cfg(test)]
fn live_share() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}

#[cfg(test)]
fn refuse_live(dir: &Path) -> eyre::Result<()> {
    let live = live_share();
    if dir == live {
        return Err(eyre::eyre!("refusing to open the live catalog"));
    }
    if dir.exists() && live.exists() && std::fs::canonicalize(dir)? == std::fs::canonicalize(&live)?
    {
        return Err(eyre::eyre!("refusing to open the live catalog"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn open_default(
        services: &Services,
        pipelines: &Pipelines,
        paths: &Paths,
    ) -> eyre::Result<Owned> {
        open_owned(services, pipelines, paths, &config::Store::default())
    }

    fn must_err(result: eyre::Result<Owned>) -> eyre::Report {
        match result {
            Err(err) => err,
            Ok(_) => panic!("startup opened a database"),
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-supervisor-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            peers: dir.join("peers.redb"),
            headers: dir.join("headers.redb"),
            corpus: dir.join("corpus.redb"),
            venues: dir.join("venues.redb"),
            peer_socket: dir.join("peer.sock"),
            feeds: dir.join("feeds.redb"),
            spots: dir.join("spots.redb"),
        }
    }

    fn both() -> Services {
        Services {
            db: true,
            peer: true,
        }
    }

    fn every_pipeline() -> Pipelines {
        Pipelines {
            headers: true,
            venues: true,
            state: true,
            oracles: false,
            spots: false,
        }
    }

    fn assert_no_databases(paths: &Paths) {
        assert!(!paths.peers.exists());
        assert!(!paths.headers.exists());
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        let dir = paths.venues.parent().expect("catalog dir");
        assert!(!dir.join("entities.redb").exists());
        assert!(!dir.join("discovery.redb").exists());
        assert!(!dir.join("work.redb").exists());
        assert!(!paths.feeds.exists());
        assert!(!paths.spots.exists());
    }

    #[test]
    fn oracles_with_db_does_not_open_the_venue_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let owned = open_default(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: false,
                venues: false,
                state: false,
                oracles: true,
                spots: false,
            },
            &paths,
        )
        .unwrap();
        assert!(owned.store.is_none());
        assert_no_databases(&paths);
    }

    #[test]
    fn data_dir_paths_stay_inside_that_directory() {
        let dir = TempDir::new();
        let paths = paths_from(&dir.0, &None).unwrap();
        assert_eq!(paths.venues, dir.0.join("entities.redb"));
        assert_eq!(paths.headers, dir.0.join("headers.redb"));
        assert_eq!(paths.feeds, dir.0.join("feeds.redb"));
        assert_eq!(paths.spots, dir.0.join("spots.redb"));
        assert!(refuse_live(&dir.0).is_ok());
    }

    #[test]
    fn spots_with_db_opens_the_venue_catalog_and_leaves_the_feed_file_closed() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let owned = open_default(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: false,
                venues: false,
                state: false,
                oracles: false,
                spots: true,
            },
            &paths,
        )
        .unwrap();
        assert!(owned.store.is_some());
        assert!(dir.0.join("entities.redb").exists());
        assert!(!paths.feeds.exists());
        assert!(!paths.spots.exists());
        drop(owned);
    }

    #[test]
    fn spots_without_db_opens_nothing() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let err = must_err(open_default(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines {
                headers: false,
                venues: false,
                state: false,
                oracles: false,
                spots: true,
            },
            &paths,
        ));
        assert!(err.to_string().contains("spots"), "{err}");
        assert_no_databases(&paths);
    }

    #[test]
    fn state_without_db_opens_nothing() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let err = must_err(open_default(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines {
                headers: false,
                venues: false,
                state: true,
                oracles: false,
                spots: false,
            },
            &paths,
        ));
        assert!(err.to_string().contains("state"), "{err}");
        assert_no_databases(&paths);
    }

    #[test]
    fn a_pipeline_missing_its_service_opens_nothing() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let headers = must_err(open_default(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: true,
                venues: false,
                state: false,
                oracles: false,
                spots: false,
            },
            &paths,
        ));
        assert!(headers.to_string().contains("headers"), "{headers}");
        assert_no_databases(&paths);
        let venues = must_err(open_default(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines {
                headers: false,
                venues: true,
                state: false,
                oracles: false,
                spots: false,
            },
            &paths,
        ));
        assert!(venues.to_string().contains("venues"), "{venues}");
        assert_no_databases(&paths);
    }

    #[test]
    fn peer_service_opens_only_the_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let owned = open_default(
            &Services {
                db: false,
                peer: true,
            },
            &Pipelines::default(),
            &paths,
        )
        .unwrap();
        assert!(paths.peers.exists());
        assert!(!paths.headers.exists());
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        drop(owned);
    }

    #[test]
    fn db_service_does_not_create_the_peer_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        let owned = open_default(
            &Services {
                db: true,
                peer: false,
            },
            &Pipelines {
                headers: false,
                venues: true,
                state: false,
                oracles: false,
                spots: false,
            },
            &paths,
        )
        .unwrap();
        assert!(!paths.peers.exists());
        assert!(paths.headers.exists());
        assert!(paths.corpus.exists());
        assert!(dir.0.join("entities.redb").exists());
        assert!(dir.0.join("discovery.redb").exists());
        assert!(dir.0.join("work.redb").exists());
        assert!(!paths.venues.exists());
        drop(owned);
    }

    #[test]
    fn locked_peer_catalog_does_not_open_entity_files() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        std::fs::write(&paths.peer_socket, b"stale-peer").unwrap();
        let held = peer::open(&paths.peers).unwrap();
        let err = must_err(open_default(&both(), &every_pipeline(), &paths));
        let text = format!("{err:#}");
        assert!(text.contains("already open"), "{text}");
        assert!(!paths.headers.exists());
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        assert_eq!(std::fs::read(&paths.peer_socket).unwrap(), b"stale-peer");
        drop(held);
    }

    #[test]
    fn locked_entity_file_releases_the_peer_catalog() {
        let dir = TempDir::new();
        let paths = paths_in(&dir.0);
        std::fs::write(&paths.peer_socket, b"stale-peer").unwrap();
        let held = LocalStore::open_headers(&paths.headers).unwrap();
        let err = must_err(open_default(&both(), &every_pipeline(), &paths));
        let text = format!("{err:#}");
        assert!(text.contains("already open"), "{text}");
        assert!(!paths.corpus.exists());
        assert!(!paths.venues.exists());
        assert_eq!(std::fs::read(&paths.peer_socket).unwrap(), b"stale-peer");
        let reopened = peer::open(&paths.peers).unwrap();
        drop(reopened);
        drop(held);
    }
}
