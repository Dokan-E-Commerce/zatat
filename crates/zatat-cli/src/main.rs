#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use clap::{Parser, Subcommand};
use tokio::signal;
use tracing::{info, warn};

use zatat_config::Config;
use zatat_scaling::{LocalOnlyProvider, RedisPubSubProvider};
use zatat_ws::state::{ServerState, ServerStateInner};

/// Supervisor: re-spawn `make_task()` when the previous run ends (either
/// normally OR via a caught panic). Emits a metric + warn log on each
/// respawn so operators can alert on flapping. Exponential backoff caps
/// at 30s so a persistent bug doesn't busy-loop.
fn supervise<F, Fut>(name: &'static str, make_task: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff_ms: u64 = 500;
        loop {
            let jh = tokio::spawn(make_task());
            match jh.await {
                Ok(()) => {
                    warn!(task = name, "supervised task exited cleanly; respawning");
                    metrics::counter!("zatat_supervisor_respawns_total", "task" => name)
                        .increment(1);
                }
                Err(e) if e.is_panic() => {
                    warn!(
                        task = name,
                        "supervised task PANICKED; respawning in {backoff_ms}ms"
                    );
                    metrics::counter!("zatat_supervisor_panics_total", "task" => name).increment(1);
                }
                Err(_) => {
                    warn!(
                        task = name,
                        "supervised task was cancelled; exiting supervisor"
                    );
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(30_000);
        }
    });
}

#[derive(Parser, Debug)]
#[command(name = "zatat", version, about = "Pusher-compatible realtime server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Start {
        #[arg(long, env = "ZATAT_CONFIG", default_value = "zatat.toml")]
        config: PathBuf,
        #[arg(long)]
        debug: bool,
    },
    Restart {
        #[arg(long, env = "ZATAT_CONFIG", default_value = "zatat.toml")]
        config: PathBuf,
    },
    Ping {
        #[arg(long, env = "ZATAT_CONFIG", default_value = "zatat.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Start { config, debug } => start(&config, debug).await,
        Cmd::Restart { config } => restart(&config).await,
        Cmd::Ping { config } => ping(&config).await,
    }
}

fn init_tracing(debug: bool) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if debug {
            tracing_subscriber::EnvFilter::new("debug")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });
    if debug {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .pretty()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .init();
    }
}

/// Every WebSocket holds a file descriptor. Containers commonly start with a
/// soft limit of 1024 (Docker ≥ 25 no longer raises it), which caps a node
/// near 1,000 connections and then fails HTTP accepts too. Raise the soft
/// limit to the hard limit, as Go runtimes do, and log the result.
#[cfg(unix)]
fn raise_fd_limit() {
    use rustix::process::{getrlimit, setrlimit, Resource};
    const WARN_BELOW: u64 = 16_384;
    // Hard limit "unlimited" (macOS reports this) still has a kernel cap;
    // ask for a large finite value and fall back if refused.
    const UNLIMITED_TARGET: u64 = 1 << 20;
    let mut limit = getrlimit(Resource::Nofile);
    let before = limit.current;
    let target = limit.maximum.unwrap_or(UNLIMITED_TARGET);
    if before.is_some_and(|soft| soft < target) {
        limit.current = Some(target);
        if setrlimit(Resource::Nofile, limit).is_err() && limit.maximum.is_none() {
            limit.current = Some(10_240);
            let _ = setrlimit(Resource::Nofile, limit);
        }
    }
    let after = getrlimit(Resource::Nofile).current;
    match after {
        Some(n) if n < WARN_BELOW => warn!(
            open_files_limit = n,
            "open-file limit is low; each WebSocket needs one — raise the hard limit \
             (docker --ulimit nofile=65536:65536, systemd LimitNOFILE)"
        ),
        _ => info!(open_files_limit = ?after, previous = ?before, "open-file limit"),
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

async fn start(config_path: &Path, debug: bool) -> Result<()> {
    init_tracing(debug);
    raise_fd_limit();
    let config =
        Config::load(config_path).with_context(|| format!("loading {}", config_path.display()))?;

    let metrics_handle = if let Some(p) = &config.server.prometheus {
        let listen: SocketAddr = p.listen.parse().context("parsing prometheus.listen")?;
        let installer = zatat_metrics::MetricsInstaller::install(listen, p.bearer_token.clone())
            .map_err(anyhow::Error::msg)?;
        Some(Arc::new(installer))
    } else {
        None
    };

    let (provider, scaling_enabled) = if let Some(s) = &config.server.scaling {
        if s.enabled {
            info!("connecting to Redis at {}:{}", s.redis.host, s.redis.port);
            let provider = RedisPubSubProvider::connect(&s.redis, s.channel.clone())
                .await
                .map_err(anyhow::Error::msg)?;
            (provider as Arc<dyn zatat_scaling::PubSubProvider>, true)
        } else {
            (Arc::new(LocalOnlyProvider) as _, false)
        }
    } else {
        (Arc::new(LocalOnlyProvider) as _, false)
    };

    let state: ServerState =
        ServerStateInner::with_provider(config.clone(), provider.clone(), scaling_enabled);
    if metrics_handle.is_some() {
        for app_id in config.apps().by_id.keys() {
            zatat_metrics::register_app(app_id.as_str());
        }
    }

    if scaling_enabled {
        let dispatcher = state.dispatcher.clone();
        let config_for_bus = config.clone();
        let provider_clone = provider.clone();
        supervise("scaling_subscriber", move || {
            let dispatcher = dispatcher.clone();
            let config_for_bus = config_for_bus.clone();
            let provider_clone = provider_clone.clone();
            async move {
                use std::sync::atomic::{AtomicU64, Ordering};
                use std::time::{Duration, Instant};
                use tokio::sync::broadcast::error::RecvError;
                let lagged_drops_total = AtomicU64::new(0);
                let mut last_warn: Option<Instant> = None;
                const LAG_WARN_INTERVAL: Duration = Duration::from_secs(10);
                match provider_clone.subscribe().await {
                    Ok(mut rx) => loop {
                        match rx.recv().await {
                            Ok(bytes) => {
                                if let Ok(env) = zatat_scaling::message::parse(&bytes) {
                                    dispatcher
                                        .handle_incoming(env, |id| config_for_bus.app_by_id(id));
                                }
                            }
                            Err(RecvError::Lagged(n)) => {
                                metrics::counter!("zatat_scaling_lagged_drops_total").increment(n);
                                let total = lagged_drops_total.fetch_add(n, Ordering::Relaxed) + n;
                                let now = Instant::now();
                                let should_warn = match last_warn {
                                    None => true,
                                    Some(t) => now.duration_since(t) >= LAG_WARN_INTERVAL,
                                };
                                if should_warn {
                                    last_warn = Some(now);
                                    warn!(
                                        skipped_this_event = n,
                                        total_drops_since_start = total,
                                        "scaling subscriber lagged; cross-node events dropped \
                                         — increase Redis pub/sub capacity or scale the consumer"
                                    );
                                }
                                continue;
                            }
                            Err(RecvError::Closed) => {
                                warn!("scaling subscriber stream ended");
                                break;
                            }
                        }
                    },
                    Err(err) => warn!(%err, "failed to subscribe to Redis bus"),
                }
            }
        });
        let snap_state = state.clone();
        supervise("presence_snapshot_publisher", move || {
            zatat_ws::tasks::presence_snapshot_publisher(snap_state.clone())
        });
        let gc_state = state.clone();
        supervise("presence_cache_gc", move || {
            zatat_ws::tasks::presence_cache_gc(gc_state.clone())
        });
    }
    let restart_state = state.clone();
    supervise("restart_signal_watcher", move || {
        zatat_ws::tasks::restart_signal_watcher(restart_state.clone())
    });
    let maint_state = state.clone();
    let tracker = state.tracker.clone();
    supervise("connection_maintenance", move || {
        zatat_ws::tasks::connection_maintenance(maint_state.clone(), tracker.clone())
    });
    // Watch zatat.toml; swap the [[apps]] table on mtime change and close
    // connections the new table no longer authorizes.
    let watch_state = state.clone();
    let config_watch_path = config_path.to_path_buf();
    supervise("watch_config_apps", move || {
        watch_config_apps(watch_state.clone(), config_watch_path.clone())
    });

    // When `server.path` is set, WS + REST routes live under that prefix;
    // `/health` stays at the root so LB probes don't need the prefix.
    let api_state = Arc::new(zatat_http::routes::ApiStateInner {
        config: config.clone(),
        channels: state.channels.clone(),
        dispatcher: state.dispatcher.clone(),
        webhooks: state.webhooks.clone(),
    });
    let base_router =
        zatat_ws::build_router(state.clone()).merge(zatat_http::build_api_router(api_state));
    let prefix = config.server.path.trim_end_matches('/').to_string();
    let app_router = if prefix.is_empty() {
        base_router
    } else {
        axum::Router::new()
            .route("/health", axum::routing::get(zatat_ws::router::health))
            .route("/up", axum::routing::get(zatat_ws::router::health))
            .with_state(state.clone())
            .nest(&prefix, base_router)
    };

    if let Some(handle) = metrics_handle.clone() {
        let listen = handle.listen_addr();
        // Bind before serving traffic: a node that cannot expose metrics
        // must fail to start rather than run unobserved.
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding metrics listener on {listen}"))?;
        let router: Router = Router::new().route(
            "/metrics",
            get({
                let h = handle.clone();
                move |headers: axum::http::HeaderMap| {
                    let h = h.clone();
                    async move {
                        let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
                        if !h.authorize(auth) {
                            return (axum::http::StatusCode::UNAUTHORIZED, String::new());
                        }
                        (axum::http::StatusCode::OK, h.render())
                    }
                }
            }),
        );
        info!(%listen, "metrics listener up");
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                warn!(%err, "metrics listener stopped");
            }
        });
    }

    let listen_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .context("parsing server.host:port")?;
    info!(%listen_addr, "zatat listening");

    // Graceful shutdown contract (critical for zero-downtime deploys):
    //   1. SIGTERM / SIGINT received, or the restart file was touched.
    //   2. state.shutdown_now() — enter draining: /health answers 503 so the
    //      LB stops routing here, new upgrades are refused, and every live
    //      WS connection gets pusher_close(1001) so clients reconnect.
    //   3. Short pause (2s) so those close frames physically leave the
    //      TCP buffer before we stop accepting writes.
    //   4. Ask the HTTP listener to drain — it stops accepting new
    //      connections but WAITS for in-flight request handlers (e.g.
    //      Laravel POST /events) to return their responses. Up to 30s.
    //   5. Flush the cross-node publish queue and webhook queue, bounded.
    //
    // The previous `tokio::select!` pattern bypassed step 4 — when ctrl_c
    // fired, the serve future was dropped mid-flight and any in-flight
    // HTTP request got aborted, which showed up as `cURL error 28 / 0 bytes
    // received` on the publisher side (~1k failed Laravel publishes during
    // a single bad rolling deploy on prod, 2026-04-18).
    const GRACEFUL_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    const WS_CLOSE_FLUSH_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
    const OUTBOUND_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    // Resolves once shutdown was requested by a signal or the restart file,
    // after draining has begun and close frames had time to flush.
    let begin_shutdown = {
        let state = state.clone();
        async move {
            tokio::select! {
                _ = shutdown_signal() => info!("[shutdown] signal received"),
                _ = state.draining() => info!("[shutdown] restart requested"),
            }
            info!("[shutdown] draining — closing WS connections with 1001");
            state.shutdown_now();
            tokio::time::sleep(WS_CLOSE_FLUSH_DELAY).await;
        }
    };

    if let Some(tls) = &config.server.tls {
        // rustls 0.23 needs an explicit crypto provider. idempotent if already set.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .context("loading TLS cert/key")?;
        tokio::spawn(watch_tls_reload(
            rustls_config.clone(),
            tls.cert.clone(),
            tls.key.clone(),
        ));
        let handle = axum_server::Handle::new();
        let handle_for_shutdown = handle.clone();
        tokio::spawn(async move {
            begin_shutdown.await;
            info!(
                timeout_s = GRACEFUL_HTTP_TIMEOUT.as_secs(),
                "[shutdown] asking HTTP listener to drain in-flight requests"
            );
            handle_for_shutdown.graceful_shutdown(Some(GRACEFUL_HTTP_TIMEOUT));
        });
        axum_server::bind_rustls(listen_addr, rustls_config)
            .handle(handle)
            .serve(app_router.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .context("tls server")?;
    } else {
        let listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .context("bind")?;
        let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel::<()>();
        let serve = axum::serve(
            listener,
            app_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            begin_shutdown.await;
            info!(
                timeout_s = GRACEFUL_HTTP_TIMEOUT.as_secs(),
                "[shutdown] HTTP listener draining in-flight requests"
            );
            let _ = drain_started_tx.send(());
        });
        // axum::serve has no drain timeout of its own; bound it here so a
        // stuck request cannot hold the process past the deploy's grace.
        let drain_deadline = async move {
            if drain_started_rx.await.is_ok() {
                tokio::time::sleep(GRACEFUL_HTTP_TIMEOUT).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            result = serve => result.context("serve")?,
            _ = drain_deadline => {
                warn!(
                    timeout_s = GRACEFUL_HTTP_TIMEOUT.as_secs(),
                    "[shutdown] HTTP drain timed out; dropping remaining requests"
                );
            }
        }
    }

    drain_outbound(&state, OUTBOUND_DRAIN_TIMEOUT).await;
    info!("shutdown complete");
    Ok(())
}

/// Waits (bounded) for queued cross-node publishes and webhook deliveries
/// to leave the process, so events accepted just before shutdown are not
/// silently lost with the runtime.
async fn drain_outbound(state: &ServerState, timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let publishes = state.dispatcher.pending_publishes();
        let webhooks = state.webhooks.pending();
        if publishes == 0 && webhooks == 0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                publishes,
                webhooks, "[shutdown] outbound queues not empty at deadline; exiting anyway"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Polls zatat.toml for mtime changes every N seconds (default 5,
/// override via `ZATAT_APPS_RELOAD_INTERVAL_S`). On change, re-reads the
/// file plus `ZATAT_*` overrides and atomically swaps the apps table.
/// Existing connections use the new settings for later actions; a removed
/// or re-keyed app's connections, and connections from origins the app no
/// longer allows, are closed.
async fn watch_config_apps(state: ServerState, path: PathBuf) {
    let interval_s = std::env::var("ZATAT_APPS_RELOAD_INTERVAL_S")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_s));
    tick.tick().await;
    let mut last = std::time::SystemTime::now();
    loop {
        tick.tick().await;
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime <= last {
            continue;
        }
        match state.config.reload_apps_from(&path) {
            Ok(reload) => {
                last = mtime;
                info!(apps = reload.loaded, revoked = reload.revoked.len(), file = %path.display(), "apps config reloaded");
                for app_id in state.config.apps().by_id.keys() {
                    zatat_metrics::register_app(app_id.as_str());
                }
                zatat_ws::tasks::apply_apps_reload(&state, &reload);
            }
            Err(err) => warn!(%err, "apps reload failed; keeping previous apps"),
        }
    }
}

/// Polls the cert + key files every 30s; when either mtime advances past
/// the last observed value, reloads the live `RustlsConfig` in place.
/// New connections use the new cert; in-flight connections are undisturbed.
async fn watch_tls_reload(
    config: axum_server::tls_rustls::RustlsConfig,
    cert_path: String,
    key_path: String,
) {
    let initial = std::time::SystemTime::now();
    let mut last_mtime = initial;
    let interval_s = std::env::var("ZATAT_TLS_RELOAD_INTERVAL_S")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_s));
    tick.tick().await;
    loop {
        tick.tick().await;
        let latest = match (
            tokio::fs::metadata(&cert_path).await,
            tokio::fs::metadata(&key_path).await,
        ) {
            (Ok(a), Ok(b)) => {
                let am = a.modified().unwrap_or(initial);
                let bm = b.modified().unwrap_or(initial);
                am.max(bm)
            }
            _ => continue,
        };
        if latest > last_mtime {
            match config.reload_from_pem_file(&cert_path, &key_path).await {
                Ok(()) => {
                    last_mtime = latest;
                    info!(cert = %cert_path, "TLS cert reloaded");
                }
                Err(err) => warn!(%err, "TLS reload failed; keeping previous cert"),
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

async fn restart(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let path = &config.server.restart_signal_file;
    tokio::fs::write(
        path,
        format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        ),
    )
    .await
    .with_context(|| format!("touching {path}"))?;
    println!("restart signal written to {path}");
    Ok(())
}

/// A bind address (e.g. `0.0.0.0` or `::`) tells the server to listen on
/// every interface — it isn't itself something a client can connect to.
/// For `ping` we substitute the loopback address so the check actually
/// dials a reachable endpoint.
fn connect_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    }
}

async fn ping(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let url = format!(
        "http://{}:{}/health",
        config.server.host, config.server.port
    );
    let connect_addr = format!(
        "{}:{}",
        connect_host(&config.server.host),
        config.server.port
    );
    match tokio::net::TcpStream::connect(connect_addr).await {
        Ok(_) => {
            println!("{url}: reachable");
            Ok(())
        }
        Err(err) => anyhow::bail!("{url} unreachable: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Regression: before `supervise`, a panicking background task died
    /// silently and was never restarted. This test proves that a task that
    /// panics on its first call is invoked again on respawn. Uses real
    /// time (tokio test-util feature isn't enabled in this workspace), so
    /// it has to wait through the 500ms backoff.
    #[tokio::test]
    async fn supervise_respawns_on_panic() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        super::supervise("panic-test", move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    panic!("intentional panic in supervised task (test)");
                }
                // Second call: sleep forever so we don't respawn again.
                std::future::pending::<()>().await;
            }
        });
        // Supervisor sleeps 500ms before respawn; wait a bit longer.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let seen = counter.load(Ordering::Relaxed);
        assert!(
            seen >= 2,
            "supervisor should respawn after a panic; only {seen} invocations observed"
        );
    }

    /// A task that exits cleanly also gets respawned — the critical
    /// background loops never "finish" in normal operation, so an orderly
    /// exit is itself a signal something went wrong.
    #[tokio::test]
    async fn supervise_respawns_on_clean_exit() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        super::supervise("clean-exit-test", move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    // First call: return immediately (clean exit).
                    return;
                }
                std::future::pending::<()>().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(counter.load(Ordering::Relaxed) >= 2);
    }
}
