//! List walks, corpus import, and history.
//!
//! Cache rows are `put_bodies` / `put_blocks` on the store, awaited before
//! `commit_chunk`.

use crate::{Sink, Source};
use ipc::HistoryPeer;
use std::sync::{Arc, Mutex};
use venues::{execute, RunOpts, RunReport};

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
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> eyre::Result<()> {
        // Same databases a `StoreSink` would write. A second push commits twice.
        let _ = sink;
        let report = execute(&self.store, self.peer.as_ref(), &self.opts, stop).await?;
        *self.shared.lock().expect("report") = report;
        Ok(())
    }
}
