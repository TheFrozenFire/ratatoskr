//! List walks, corpus import, and history.
//!
//! Cache rows are `put_bodies` / `put_blocks` on the store, awaited before
//! `commit_chunk`. The task repeats. The corpus fold runs on the first pass.
//! Later passes read lists, blooms, and token details again as the header
//! cursor advances. The state phase loops inside `execute` and is not
//! repeated here.

use crate::{Sink, Source};
use ipc::HistoryPeer;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use venues::{execute, RunOpts, RunPhase, RunReport};

/// Idle time between discovery passes once the current headers are read.
const FOLLOW: Duration = Duration::from_secs(60);

pub struct VenueSource<S, P> {
    store: S,
    peer: Option<P>,
    opts: RunOpts,
    shared: Arc<Mutex<RunReport>>,
}

impl<S, P> VenueSource<S, P> {
    pub fn new(store: S, peer: Option<P>, opts: RunOpts) -> Self {
        Self {
            store,
            peer,
            opts,
            shared: Arc::new(Mutex::new(RunReport::default())),
        }
    }

    /// Clone this before [`Source::run`] consumes the source.
    pub fn report(&self) -> Arc<Mutex<RunReport>> {
        Arc::clone(&self.shared)
    }
}

impl<S, P> Source for VenueSource<S, P>
where
    S: venues::EntityStore + Clone + 'static,
    P: HistoryPeer + Clone + 'static,
{
    fn name(&self) -> &str {
        "venues"
    }

    async fn run(
        self,
        sink: &mut dyn Sink,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> eyre::Result<()> {
        // Same databases a `StoreSink` would write. A second push commits twice.
        let _ = sink;
        if self.opts.phase == RunPhase::State {
            let report = execute(&self.store, self.peer.as_ref(), &self.opts, stop).await?;
            *self.shared.lock().expect("report") = report;
            return Ok(());
        }
        let mut opts = self.opts.clone();
        loop {
            if venues::hold_for_pause(&stop).await {
                return Ok(());
            }
            let report = execute(&self.store, self.peer.as_ref(), &opts, stop.clone()).await?;
            let corpus_error = report
                .list_errors
                .iter()
                .any(|err| err.starts_with("corpus:"));
            *self.shared.lock().expect("report") = report;
            if !corpus_error {
                opts.no_corpus = true;
            }
            if venues::hold_for_pause(&stop).await {
                return Ok(());
            }
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(FOLLOW) => {}
            }
        }
    }
}
