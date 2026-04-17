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

#[derive(Parser, Debug)]
#[command(name = "zatat", version, about = "Pusher-compatible realtime server")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Start {
        #[arg(long, default_value = "zatat.toml")]
        config: PathBuf,
        #[arg(long)]
        debug: bool,
    },
    Restart {
        #[arg(long, default_value = "zatat.toml")]
        config: PathBuf,
    },
    Ping {
        #[arg(long, default_value = "zatat.toml")]
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

async fn start(config_path: &Path, debug: bool) -> Result<()> {
    init_tracing(debug);
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

    if scaling_enabled {
        let dispatcher = state.dispatcher.clone();
        let config_for_bus = config.clone();
        let provider_clone = provider.clone();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            match provider_clone.subscribe().await {
                Ok(mut rx) => loop {
                    match rx.recv().await {
                        Ok(bytes) => {
                            if let Ok(env) = zatat_scaling::message::parse(&bytes) {
                                dispatcher.handle_incoming(env, |id| config_for_bus.app_by_id(id));
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            warn!(skipped = n, "scaling subscriber lagged; messages missed");
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
        });
        tokio::spawn(zatat_ws::tasks::presence_snapshot_publisher(state.clone()));
        tokio::spawn(zatat_ws::tasks::presence_cache_gc(state.clone()));
    }
    tokio::spawn(zatat_ws::tasks::restart_signal_watcher(state.clone()));
    tokio::spawn(zatat_ws::tasks::connection_maintenance(
        state.clone(),
        state.tracker.clone(),
    ));
    // Watch zatat.toml; swap the [[apps]] table on mtime change.
    // Live WS connections keep their captured AppArc and are unaffected.
    tokio::spawn(watch_config_apps(config.clone(), config_path.to_path_buf()));

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
            .route("/health", axum::routing::get(|| async { "ok" }))
            .nest(&prefix, base_router)
    };

    if let Some(handle) = metrics_handle.clone() {
        let listen = handle.listen_addr();
        tokio::spawn(async move {
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
            let listener = match tokio::net::TcpListener::bind(listen).await {
                Ok(l) => l,
                Err(err) => {
                    warn!(%err, "failed to bind metrics listener");
                    return;
                }
            };
            let _ = axum::serve(listener, router).await;
        });
    }

    let listen_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .context("parsing server.host:port")?;
    info!(%listen_addr, "zatat listening");

    let shutdown = state.clone();
    let ctrl_c = async move {
        shutdown_signal().await;
        shutdown.shutdown_now();
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
        let listener = axum_server::bind_rustls(listen_addr, rustls_config);
        tokio::select! {
            _ = ctrl_c => {}
            res = listener.serve(app_router.into_make_service_with_connect_info::<SocketAddr>()) => {
                res.context("tls server")?;
            }
        }
    } else {
        let listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .context("bind")?;
        tokio::select! {
            _ = ctrl_c => {}
            res = axum::serve(listener, app_router.into_make_service_with_connect_info::<SocketAddr>()) => {
                res.context("serve")?;
            }
        }
    }

    info!("shutdown complete");
    Ok(())
}

/// Polls zatat.toml for mtime changes every N seconds (default 5,
/// override via `ZATAT_APPS_RELOAD_INTERVAL_S`). On change, re-parses
/// the file and atomically swaps the apps table. Existing WS
/// connections keep the `AppArc` they captured at upgrade time, so
/// they observe the apps config they connected with — no disconnect.
async fn watch_config_apps(config: Config, path: PathBuf) {
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
        if mtime > last {
            match config.reload_apps_from(&path) {
                Ok(n) => {
                    last = mtime;
                    info!(apps = n, file = %path.display(), "apps config reloaded");
                }
                Err(err) => warn!(%err, "apps reload failed; keeping previous apps"),
            }
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

async fn ping(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let url = format!(
        "http://{}:{}/health",
        config.server.host, config.server.port
    );
    match tokio::net::TcpStream::connect(format!("{}:{}", config.server.host, config.server.port))
        .await
    {
        Ok(_) => {
            println!("{url}: reachable");
            Ok(())
        }
        Err(err) => anyhow::bail!("{url} unreachable: {err}"),
    }
}
