//! Signals, the peer task, the pipeline tasks, and shutdown.

use crate::cli::{resolve_rpcs, Args, Pipelines};
use crate::layout::open_owned;
use crate::sampler::spawn_sampler;
use peer::LocalPeer;
use pipeline::{HeaderSource, Source, StoreSink, VenueSource};
use std::path::PathBuf;
use store::LocalStore;
use tokio::sync::watch;
use tokio::task::JoinSet;
use venues::{RunOpts, RunPhase};

pub(crate) async fn supervise(args: Args, loaded: config::Config) -> eyre::Result<()> {
    let metrics = args
        .metrics
        .clone()
        .unwrap_or_else(|| loaded.process.metrics.clone());
    crate::export::serve(&metrics)?;
    let ready = match crate::oneshot::run(&args, &loaded).await? {
        Some(ready) => ready,
        None => return Ok(()),
    };
    let crate::oneshot::Prepared {
        services,
        pipelines,
        egress,
        paths,
        caches,
    } = ready;
    let rpcs = resolve_rpcs(&args.rpc, pipelines.state, &loaded.network)?;
    let oracle_rpcs = rpcs.clone();
    let mut owned = open_owned(&services, &pipelines, &paths, &caches)?;
    let local = owned.peer.as_ref().map(peer::OpenedPeer::local);
    let mut peer_settings = loaded.peer.clone();
    if let Some(target) = args.peers {
        peer_settings.target = target;
    }
    let prepared = match owned.peer.take() {
        Some(opened) => Some(peer::prepare(
            opened,
            &peer::PeerConfig {
                socket: paths.peer_socket.clone(),
                catalog: paths.peers.clone(),
                egress,
                peer: peer_settings,
                network: loaded.network.clone(),
            },
        )?),
        None => None,
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let signal_tx = stop_tx.clone();
    tokio::spawn(async move {
        if let Err(err) = shutdown_signal().await {
            observability::error!(%err, "shutdown signal");
        }
        let _ = signal_tx.send(true);
    });
    tokio::spawn(async move {
        if let Err(err) = pause_signal().await {
            observability::error!(%err, "pause signal");
        }
    });
    let peer_task = prepared.map(|prepared| {
        let stop = stop_rx.clone();
        tokio::spawn(async move { peer::run(prepared, stop).await })
    });
    let store = owned.store.clone();
    let opts = RunOpts {
        rpcs,
        only: args.only,
        no_corpus: args.no_corpus,
        corpus_available: !args.no_corpus,
        phase: RunPhase::All,
        pin_block: args.pin_block,
        until_idle: args.until_idle,
        venues: loaded.venues.clone(),
    };
    if pipelines.state {
        if let Some(store) = store.clone() {
            spawn_sampler(store, loaded.process.clone());
        }
    }
    let pipelines_started = pipelines.headers
        || pipelines.venues
        || pipelines.state
        || pipelines.oracles
        || pipelines.spots;
    let mut peer_task = peer_task;
    let mut stop_rx = stop_rx;
    let mut failed = tokio::task::LocalSet::new()
        .run_until(async {
            let mut tasks = spawn_pipelines(
                &pipelines,
                store.clone(),
                local,
                opts,
                stop_rx.clone(),
                paths.feeds.clone(),
                paths.spots.clone(),
                oracle_rpcs,
                &loaded,
            );
            wait_for_stop(
                &mut tasks,
                &mut peer_task,
                pipelines_started,
                &stop_tx,
                &mut stop_rx,
            )
            .await
        })
        .await;
    let _ = stop_tx.send(true);
    if let Some(task) = peer_task {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => note_failure(&mut failed, err),
            Err(err) => note_failure(&mut failed, eyre::eyre!("peer task: {err}")),
        }
    }
    drop(store);
    drop(owned);
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_pipelines(
    pipelines: &Pipelines,
    store: Option<LocalStore>,
    local: Option<LocalPeer>,
    opts: RunOpts,
    stop: watch::Receiver<bool>,
    feeds: PathBuf,
    spots_path: PathBuf,
    rpcs: Vec<String>,
    loaded: &config::Config,
) -> JoinSet<eyre::Result<()>> {
    let mut tasks = JoinSet::new();
    if pipelines.headers {
        let peer = local.clone().expect("headers needs both db and peer");
        let store = store.clone().expect("headers needs both db and peer");
        let stop = stop.clone();
        let sync = loaded.pipeline.clone();
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            HeaderSource::with(peer, store, sync)
                .run(&mut sink, stop)
                .await
        });
    }
    if pipelines.venues {
        let store = store.clone().expect("venues needs db");
        let venue_peer = local.clone();
        let venue_opts = opts.clone();
        let stop = stop.clone();
        let follow = loaded.pipeline.follow();
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            VenueSource::with(store, venue_peer, venue_opts, follow)
                .run(&mut sink, stop)
                .await
        });
    }
    if pipelines.spots {
        let store = store.clone().expect("spots needs db");
        let stop = stop.clone();
        let spot_cache = loaded.store.spots;
        let spot_settings = loaded.spots.clone();
        tasks.spawn_local(async move {
            let book = store::service::Spots::open_with(&spots_path, spot_cache)?;
            spots::run(book, stop, spot_settings, move || {
                spots::conduit_edges(&store)
            })
            .await
        });
    }
    if pipelines.state {
        let store = store.expect("state needs db");
        let stop = stop.clone();
        let mut opts = opts;
        opts.phase = RunPhase::State;
        let follow = loaded.pipeline.follow();
        tasks.spawn_local(async move {
            let mut sink = StoreSink::new(store.clone());
            VenueSource::with(store, local, opts, follow)
                .run(&mut sink, stop)
                .await
        });
    }
    if pipelines.oracles {
        let stop = stop.clone();
        let feed_cache = loaded.store.feeds;
        let oracle_settings = loaded.oracles.clone();
        tasks.spawn_local(async move {
            let book = store::service::Feeds::open_with(&feeds, feed_cache)?;
            oracles::run(book, rpcs, oracle_settings, stop).await
        });
    }
    tasks
}

fn note_failure(failed: &mut Option<eyre::Report>, err: eyre::Report) {
    observability::error!(%err);
    if failed.is_none() {
        *failed = Some(err);
    }
}

async fn wait_for_stop(
    tasks: &mut JoinSet<eyre::Result<()>>,
    peer_task: &mut Option<tokio::task::JoinHandle<eyre::Result<()>>>,
    pipelines_started: bool,
    stop_tx: &watch::Sender<bool>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Option<eyre::Report> {
    let mut failed = None;
    loop {
        if tasks.is_empty() && *stop_rx.borrow() {
            break;
        }
        if tasks.is_empty() && pipelines_started && failed.is_none() && !*stop_rx.borrow() {
            break;
        }
        if tasks.is_empty() && peer_task.is_none() {
            break;
        }
        tokio::select! {
            biased;
            joined = tasks.join_next(), if !tasks.is_empty() => {
                match joined {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(err))) => {
                        observability::error!(%err, "pipeline");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    Some(Err(err)) => {
                        let err = eyre::eyre!("pipeline task: {err}");
                        observability::error!(%err, "pipeline task");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    None => {}
                }
            }
            joined = async { peer_task.as_mut().unwrap().await }, if peer_task.is_some() => {
                *peer_task = None;
                match joined {
                    Ok(Ok(())) if *stop_rx.borrow() => {}
                    Ok(Ok(())) => {
                        let err = eyre::eyre!("peer stopped");
                        observability::error!(%err, "peer");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    Ok(Err(err)) => {
                        observability::error!(%err, "peer");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                    Err(err) => {
                        let err = eyre::eyre!("peer maintain: {err}");
                        observability::error!(%err, "peer task");
                        if failed.is_none() {
                            failed = Some(err);
                        }
                        let _ = stop_tx.send(true);
                    }
                }
            }
            changed = stop_rx.changed(), if !*stop_rx.borrow() => {
                if changed.is_err() {
                    let _ = stop_tx.send(true);
                }
            }
        }
    }
    failed
}

async fn shutdown_signal() -> eyre::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

async fn pause_signal() -> eyre::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut pause = signal(SignalKind::user_defined1())?;
    let mut resume = signal(SignalKind::user_defined2())?;
    loop {
        tokio::select! {
            _ = pause.recv() => venues::pause_work(),
            _ = resume.recv() => venues::resume_work(),
        }
    }
}
