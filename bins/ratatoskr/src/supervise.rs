//! Signals, the peer task, the pipeline tasks, and shutdown.

use crate::cli::{resolve_rpcs, Args, Pipelines};
use crate::layout::open_owned;
use crate::sampler::spawn_sampler;
use peer::LocalPeer;
use pipeline::{HeaderSource, Source, StoreSink, VenueSource};
use std::path::PathBuf;
use std::time::Duration;
use store::LocalStore;
use tokio::sync::watch;
use tokio::task::JoinSet;
use venue::{RunOpts, RunPhase};

/// Blocking reads do not observe the stop flag. The process exits if they
/// are still running when this elapses. A second interrupt exits at once.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub(crate) async fn supervise(args: Args, loaded: config::Config) -> eyre::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    install_signals(stop_tx.clone())?;
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
        archival: loaded.archival,
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
            Err(err) if err.is_cancelled() => {}
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
            spot::run(book, store, stop, spot_settings).await
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
            oracle::run(book, rpcs, oracle_settings, stop).await
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
        if *stop_rx.borrow() {
            tasks.abort_all();
            if let Some(task) = peer_task.as_ref() {
                task.abort();
            }
        }
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
                    Some(Err(err)) if err.is_cancelled() && *stop_rx.borrow() => {}
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
                    Err(err) if err.is_cancelled() && *stop_rx.borrow() => {}
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
                if changed.is_err() || *stop_rx.borrow() {
                    tasks.abort_all();
                    if let Some(task) = peer_task.as_ref() {
                        task.abort();
                    }
                }
            }
        }
    }
    failed
}

fn install_signals(stop: watch::Sender<bool>) -> eyre::Result<()> {
    use signal_hook::consts::signal::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM, SIGUSR1, SIGUSR2])?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let mut shutting_down = false;
            for signal in signals.forever() {
                match signal_action(signal, shutting_down) {
                    SignalAction::Shutdown => {
                        shutting_down = true;
                        observability::info!(signal, "shutdown");
                        let _ = stop.send(true);
                        let code = exit_code(signal);
                        if std::thread::Builder::new()
                            .name("shutdown".into())
                            .spawn(move || {
                                std::thread::sleep(SHUTDOWN_GRACE);
                                eprintln!("ratatoskr: shutdown did not finish, exiting");
                                std::process::exit(code);
                            })
                            .is_err()
                        {
                            eprintln!("ratatoskr: shutdown did not finish, exiting");
                            std::process::exit(code);
                        }
                    }
                    SignalAction::Exit => {
                        eprintln!("ratatoskr: second shutdown signal, exiting");
                        std::process::exit(exit_code(signal));
                    }
                    SignalAction::Pause => venue::pause_work(),
                    SignalAction::Resume => venue::resume_work(),
                    SignalAction::Ignore => {}
                }
            }
        })
        .map_err(|err| eyre::eyre!("signal thread: {err}"))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalAction {
    Shutdown,
    Exit,
    Pause,
    Resume,
    Ignore,
}

fn signal_action(signal: i32, shutting_down: bool) -> SignalAction {
    use signal_hook::consts::signal::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
    match signal {
        SIGINT | SIGTERM if shutting_down => SignalAction::Exit,
        SIGINT | SIGTERM => SignalAction::Shutdown,
        SIGUSR1 => SignalAction::Pause,
        SIGUSR2 => SignalAction::Resume,
        _ => SignalAction::Ignore,
    }
}

fn exit_code(signal: i32) -> i32 {
    128 + signal
}

#[cfg(test)]
mod tests {
    use super::{exit_code, signal_action, SignalAction};
    use signal_hook::consts::signal::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};

    #[test]
    fn the_first_interrupt_asks_the_process_to_stop_and_the_second_exits() {
        assert_eq!(signal_action(SIGINT, false), SignalAction::Shutdown);
        assert_eq!(signal_action(SIGTERM, false), SignalAction::Shutdown);
        assert_eq!(signal_action(SIGINT, true), SignalAction::Exit);
        assert_eq!(signal_action(SIGTERM, true), SignalAction::Exit);
        assert_eq!(exit_code(SIGINT), 130);
        assert_eq!(exit_code(SIGTERM), 143);
    }

    #[test]
    fn user_signals_pause_and_resume_work() {
        assert_eq!(signal_action(SIGUSR1, false), SignalAction::Pause);
        assert_eq!(signal_action(SIGUSR2, false), SignalAction::Resume);
        assert_eq!(signal_action(SIGUSR1, true), SignalAction::Pause);
    }
}
