//! cairnd: lightweight OpenTofu automation server.

mod api;
mod app;
mod auth;
mod blob;
mod crypto;
mod db;
mod deps;
mod engine;
mod events;
mod http;
mod notify;
mod oidc;
mod policy;
mod registry;
mod registry_gpg;
mod runner;
mod schedule;
mod store;
mod util;
mod vcs;

use std::sync::Arc;
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
    for sub in ["logs", "work", "engines", "cache/plugins"] {
        std::fs::create_dir_all(cfg.data.join(sub)).with_context(|| format!("creating {}", cfg.data.display()))?;
    }
    let db = db::Db::open(&cfg.data.join("cairn.db"), cpus.min(4))?;
    let authz = auth::Authz::load(&db)?;
    let secrets = Arc::new(crypto::SecretBox::load(&cfg.data, cfg.secret_key.as_deref())?);
    let blobs = blob::BlobStore::open(
        cfg.blob_store.as_deref(),
        cfg.data.join("blobs"),
        (!cfg.no_blob_encryption).then(|| secrets.clone()),
    )?;
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
    if cfg.site_token.is_none() && authz.snapshot().tokens.is_empty() && cfg.oidc_issuer.is_none() {
        tracing::warn!("no CAIRN_SITE_TOKEN, API tokens or OIDC issuer: nobody can log in");
    }
    let oidc = cfg.oidc_issuer.as_ref().map(|issuer| {
        oidc::Oidc::new(oidc::OidcConfig {
            issuer: issuer.clone(),
            client_id: cfg.oidc_client_id.clone(),
            client_secret: cfg.oidc_client_secret.clone(),
            groups_claim: cfg.oidc_groups_claim.clone(),
            admin_group: cfg.oidc_admin_group.clone(),
            scopes: cfg.oidc_scopes.clone(),
            audiences: cfg.oidc_audiences.clone(),
        })
    });

    let app = Arc::new(app::App {
        hub: events::Hub::new(cfg.data.join("logs")),
        cfg: cfg.clone(),
        db,
        engines,
        secrets,
        blobs,
        sched: runner::Scheduler::new(concurrency),
        metrics: Default::default(),
        authz,
        oidc,
        policies: Default::default(),
        jwt_cache: Default::default(),
        run_tokens: Default::default(),
        schedule_changed: Default::default(),
        url_key,
    });

    let blob_desc = app.blobs.describe();
    let listener = tokio::net::TcpListener::bind(cfg.listen).await.with_context(|| format!("binding {}", cfg.listen))?;
    tokio::spawn(runner::scheduler_loop(app.clone()));
    tokio::spawn(notify::sender(app.clone()));
    tokio::spawn(api::vcs::reporter(app.clone()));
    tokio::spawn(schedule::timer(app.clone()));
    tracing::info!(
        listen = %cfg.listen,
        data = %cfg.data.display(),
        concurrency,
        tls = cfg.tls_cert.is_some(),
        blobs = %blob_desc,
        startup_ms = started.elapsed().as_millis() as u64,
        "cairnd ready"
    );
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        tracing::info!("shutting down");
    };
    let router = api::router(app);
    match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(cert), Some(key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .with_context(|| format!("loading TLS certificate {}", cert.display()))?;
            let handle = axum_server::Handle::new();
            let h2 = handle.clone();
            tokio::spawn(async move {
                shutdown.await;
                h2.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
            });
            let std_listener = listener.into_std()?;
            axum_server::from_tcp_rustls(std_listener, tls)?
                .handle(handle)
                .serve(router.into_make_service())
                .await?;
        }
        _ => axum::serve(listener, router).with_graceful_shutdown(shutdown).await?,
    }
    Ok(())
}
