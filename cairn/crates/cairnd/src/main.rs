//! cairnd: lightweight OpenTofu automation server.

mod api;
mod app;
mod crypto;
mod db;
mod engine;
mod events;
mod runner;
mod store;
mod util;

use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    let started = Instant::now();
    let cfg = app::Config::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CAIRN_LOG").unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .init();

    let cpus = effective_cpus();
    let workers = cfg.workers.unwrap_or(cpus.min(4)).max(1);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(16)
        .thread_stack_size(512 * 1024)
        .enable_all()
        .build()?
        .block_on(serve(cfg, cpus, started))
}

/// CPUs we may actually use: the smaller of the visible cores and the cgroup
/// CPU quota (v2 `cpu.max`, or v1 `cpu.cfs_quota_us`), rounded up.
fn effective_cpus() -> usize {
    let visible = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let read = |p: &str| std::fs::read_to_string(p).ok();
    let quota = if let Some(max) = read("/sys/fs/cgroup/cpu.max") {
        let mut it = max.split_whitespace();
        match (it.next().and_then(|q| q.parse::<f64>().ok()), it.next().and_then(|p| p.parse::<f64>().ok())) {
            (Some(q), Some(p)) if p > 0.0 => Some(q / p),
            _ => None,
        }
    } else {
        match (read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"), read("/sys/fs/cgroup/cpu/cpu.cfs_period_us")) {
            (Some(q), Some(p)) => match (q.trim().parse::<f64>(), p.trim().parse::<f64>()) {
                (Ok(q), Ok(p)) if q > 0.0 && p > 0.0 => Some(q / p),
                _ => None,
            },
            _ => None,
        }
    };
    match quota {
        Some(q) => visible.min(q.ceil().max(1.0) as usize),
        None => visible,
    }
}

async fn serve(cfg: app::Config, cpus: usize, started: Instant) -> Result<()> {
    for sub in ["blobs/config", "blobs/state", "logs", "work", "engines", "cache/plugins"] {
        std::fs::create_dir_all(cfg.data.join(sub)).with_context(|| format!("creating {}", cfg.data.display()))?;
    }
    let db = db::Db::open(&cfg.data.join("cairn.db"), cpus.min(4))?;
    let tokens = app::load_tokens(&db)?;
    let secrets = crypto::SecretBox::load(&cfg.data, cfg.secret_key.as_deref())?;
    let engines = engine::Engines::new(
        cfg.data.join("engines"),
        cfg.tofu_bin.clone(),
        cfg.default_engine_version.clone(),
        !cfg.no_download,
    );
    // Runs are partly I/O-bound (process start, state I/O): allow overlap even on 1 CPU.
    let concurrency = cfg.concurrency.unwrap_or(cpus.max(2)).max(1);
    let mut url_key = [0u8; 32];
    util::random_bytes(&mut url_key);
    if cfg.site_token.is_none() && tokens.is_empty() {
        tracing::warn!("no CAIRN_SITE_TOKEN and no API tokens: set CAIRN_SITE_TOKEN to be able to log in");
    }

    let app = Arc::new(app::App {
        hub: events::Hub::new(cfg.data.join("logs")),
        cfg: cfg.clone(),
        db,
        engines,
        secrets,
        sched: runner::Scheduler::new(concurrency),
        metrics: Default::default(),
        tokens: RwLock::new(tokens),
        url_key,
    });

    let listener = tokio::net::TcpListener::bind(cfg.listen).await.with_context(|| format!("binding {}", cfg.listen))?;
    tokio::spawn(runner::scheduler_loop(app.clone()));
    tracing::info!(
        listen = %cfg.listen,
        data = %cfg.data.display(),
        concurrency,
        startup_ms = started.elapsed().as_millis() as u64,
        "cairnd ready"
    );
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
