//! Refit, refresh, drop, fill, trim, sample, and summarize.

use crate::cli::{
    bind_addr, egress_from, parse_kind_list, parse_pipelines, parse_services, resolve_rpcs, Args,
    Pipelines, Services,
};
use crate::layout::{check_combination, paths_from, Paths};
use eyre::WrapErr;
use std::path::Path;

pub(crate) struct Prepared {
    pub(crate) services: Services,
    pub(crate) pipelines: Pipelines,
    pub(crate) egress: Option<eth_p2p::Egress>,
    pub(crate) paths: Paths,
    pub(crate) caches: config::Store,
}

/// Run a one-shot catalog pass when the arguments ask for one.
///
/// `Ok(None)` means that pass finished and the process should exit.
pub(crate) async fn run(args: &Args, loaded: &config::Config) -> eyre::Result<Option<Prepared>> {
    let services_text = match &args.services {
        Some(text) => text.clone(),
        None => loaded.process.services.join(","),
    };
    let pipeline_text = match &args.pipeline {
        Some(text) => text.clone(),
        None => loaded.process.pipelines.join(","),
    };
    let services = parse_services(&services_text)?;
    let pipelines = parse_pipelines(&pipeline_text)?;
    let egress = egress_from(
        bind_addr(args.bind4, loaded.peer.bind4.as_deref())?,
        bind_addr(args.bind6, loaded.peer.bind6.as_deref())?,
    )?;
    if args.refit && args.data_dir.is_none() {
        return Err(eyre::eyre!("--refit needs --data-dir"));
    }
    if args.refresh_kinds.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--refresh-kinds needs --data-dir"));
    }
    let data_dir = match &args.data_dir {
        Some(dir) => dir.clone(),
        None => config::expand_home(&loaded.process.data_dir)?,
    };
    let paths = paths_from(&data_dir, &loaded.peer.socket)?;
    let caches = loaded.store.clone();
    if args.refit {
        refit(&paths, &caches).await?;
        return Ok(None);
    }
    if let Some(spec) = args.refresh_kinds.clone() {
        refresh(&spec, &args.rpc, &paths, &caches, &loaded.network).await?;
        return Ok(None);
    }
    if args.drop_empty && args.data_dir.is_none() {
        return Err(eyre::eyre!("--drop-empty needs --data-dir"));
    }
    if args.fill.is_some() && args.copy_from.is_none() {
        return Err(eyre::eyre!("--fill needs --copy-from"));
    }
    if args.trim_to.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--trim-to needs --data-dir"));
    }
    if args.drop_empty || args.fill.is_some() || args.trim_to.is_some() {
        reshape(args, &paths, &caches).await?;
        return Ok(None);
    }
    check_combination(&services, &pipelines)?;
    if args.summarize && args.data_dir.is_none() {
        return Err(eyre::eyre!("--summarize needs --data-dir"));
    }
    if args.copy_from.is_some() && args.data_dir.is_none() {
        return Err(eyre::eyre!("--copy-from needs --data-dir"));
    }
    if let Some(src) = &args.copy_from {
        let sample_size = args.sample_size.unwrap_or(loaded.process.sample_size);
        sample(src, &paths.venues, sample_size, &caches)?;
    }
    if args.summarize {
        summarize(&paths, &caches)?;
        return Ok(None);
    }
    Ok(Some(Prepared {
        services,
        pipelines,
        egress,
        paths,
        caches,
    }))
}

async fn refit(paths: &Paths, caches: &config::Store) -> eyre::Result<()> {
    let venues_path = paths.venues.clone();
    let caches = caches.clone();
    let wrote = tokio::task::spawn_blocking(move || {
        let catalog = store::service::Catalog::open_with(&venues_path, &caches)?;
        venues::refit_curves(&catalog)
    })
    .await
    .wrap_err("refit task")??;
    println!("curves {wrote}");
    Ok(())
}

async fn refresh(
    spec: &str,
    rpc: &[String],
    paths: &Paths,
    caches: &config::Store,
    network: &config::Network,
) -> eyre::Result<()> {
    let kinds = parse_kind_list(spec)?;
    let urls = resolve_rpcs(rpc, true, network)?;
    let venues_path = paths.venues.clone();
    let caches = caches.clone();
    let (report, built) = tokio::task::spawn_blocking(move || {
        let catalog = store::service::Catalog::open_with(&venues_path, &caches)?;
        let rpc = chain::Rpc::new(urls)?;
        let report = venues::refresh_kinds(&catalog, &rpc, &kinds)?;
        let built = catalog.drain_conduits()?;
        Ok::<_, eyre::Report>((report, built))
    })
    .await
    .wrap_err("refresh task")??;
    println!("read {}", report.read);
    println!("failed {}", report.failed);
    println!("curves {}", report.curves);
    println!("conduits {built}");
    Ok(())
}

async fn reshape(args: &Args, paths: &Paths, caches: &config::Store) -> eyre::Result<()> {
    let sample_dir = paths
        .venues
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    if args.drop_empty {
        let venues_path = paths.venues.clone();
        let caches = caches.clone();
        let dropped = tokio::task::spawn_blocking(move || {
            let catalog = store::service::Catalog::open_with(&venues_path, &caches)?;
            catalog.drop_empty_pools()
        })
        .await
        .wrap_err("drop empty")??;
        venues::append_dropped_ids(&sample_dir, &dropped)?;
        println!("dropped_empty {}", dropped.len());
    }
    if let Some(add) = args.fill {
        let src = args.copy_from.clone().expect("checked");
        let venues_path = paths.venues.clone();
        let caches = caches.clone();
        let report = tokio::task::spawn_blocking(move || {
            venues::fill_identities(&src, &venues_path, add, &caches)
        })
        .await
        .wrap_err("fill identities")??;
        venues::append_fill_ids(&sample_dir, &report.ids)?;
        println!("filled {}", report.copied);
        for (kind, count) in &report.by_kind {
            println!("filled {} {count}", venues::kind_name(*kind));
        }
    }
    if let Some(target) = args.trim_to {
        let venues_path = paths.venues.clone();
        let listed = venues::read_fill_ids(&sample_dir)?;
        let caches = caches.clone();
        let (removed, left) = tokio::task::spawn_blocking(move || {
            let catalog = store::service::Catalog::open_with(&venues_path, &caches)?;
            catalog.trim_listed(target, &listed)
        })
        .await
        .wrap_err("trim sample")??;
        venues::write_fill_ids(&sample_dir, &left)?;
        println!("trimmed {removed}");
    }
    Ok(())
}

fn sample(
    src: &Path,
    venues_path: &Path,
    sample_size: u64,
    caches: &config::Store,
) -> eyre::Result<()> {
    let report = venues::sample_identities(src, venues_path, sample_size, caches)?;
    observability::info!(
        copied = report.copied,
        anchor = report.anchor,
        "identity sample"
    );
    for (kind, count) in &report.by_kind {
        observability::info!(kind = venues::kind_name(*kind), count, "sample kind");
    }
    if !report.anchor {
        observability::warn!("sample did not include the USDC/WETH 0.3% pool");
    }
    Ok(())
}

fn summarize(paths: &Paths, caches: &config::Store) -> eyre::Result<()> {
    let catalog = store::service::Catalog::open_with(&paths.venues, caches)?;
    let summary = catalog.state_summary()?;
    println!("identities {}", summary.identities);
    println!("immutable {}", summary.immutable);
    println!("mutable {}", summary.mutable);
    println!("curves {}", summary.curves);
    println!("conduits {}", summary.conduits);
    println!("tick_pages {}", summary.tick_pages);
    println!("mismatches {}", summary.mismatches);
    println!("tokens {}", summary.tokens);
    println!("jobs {}", summary.jobs);
    for (stage, count) in &summary.jobs_by_stage {
        println!("jobs stage {stage} {count}");
    }
    for (error, count) in &summary.errors {
        println!("error {count} {error}");
    }
    Ok(())
}
