#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use zatat_core::application::{
    AcceptClientEventsFrom, AppArc, Application, ApplicationError, RateLimitConfig,
};
use zatat_core::id::{AppId, AppKey};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Figment(#[from] Box<figment::Error>),
    #[error("invalid application: {0}")]
    Application(#[from] ApplicationError),
    #[error("duplicate app key `{0}`")]
    DuplicateAppKey(String),
    #[error("duplicate app id `{0}`")]
    DuplicateAppId(String),
}

impl From<figment::Error> for ConfigError {
    fn from(e: figment::Error) -> Self {
        ConfigError::Figment(Box::new(e))
    }
}

/// Live, atomically-swappable lookup tables for the currently-active apps.
/// Existing connections hold their own `AppArc` clones so removing an app
/// here never affects live traffic.
#[derive(Debug, Default)]
pub struct AppIndex {
    pub by_id: HashMap<AppId, AppArc>,
    pub by_key: HashMap<AppKey, AppArc>,
}

#[derive(Clone)]
pub struct Config {
    pub server: ServerConfig,
    apps: std::sync::Arc<arc_swap::ArcSwap<AppIndex>>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let idx = self.apps();
        f.debug_struct("Config")
            .field("server", &self.server)
            .field("apps", &idx.by_id.len())
            .finish()
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("ZATAT_").split("__"))
            .extract()?;
        raw.compile()
    }

    pub fn load_from_env() -> Result<Self, ConfigError> {
        let raw: RawConfig = Figment::new()
            .merge(Env::prefixed("ZATAT_").split("__"))
            .extract()?;
        raw.compile()
    }

    /// Current apps table. Cheap to call (clones an `Arc`).
    pub fn apps(&self) -> std::sync::Arc<AppIndex> {
        self.apps.load_full()
    }

    pub fn app_by_id(&self, id: &AppId) -> Option<AppArc> {
        self.apps.load().by_id.get(id).cloned()
    }

    pub fn app_by_key(&self, key: &AppKey) -> Option<AppArc> {
        self.apps.load().by_key.get(key).cloned()
    }

    /// Re-parse `path` and atomically replace the apps table. The `server`
    /// block is intentionally NOT re-applied — host/port/TLS/Redis changes
    /// still need a process restart. Returns the number of apps loaded.
    pub fn reload_apps_from(&self, path: &Path) -> Result<usize, ConfigError> {
        let raw: RawConfig = Figment::new().merge(Toml::file(path)).extract()?;
        let next = raw.build_app_index()?;
        let n = next.by_id.len();
        self.apps.store(std::sync::Arc::new(next));
        Ok(n)
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub hostname: Option<String>,
    pub max_request_size: u64,
    pub restart_signal_file: String,
    pub restart_poll_interval: Duration,
    pub tls: Option<TlsConfig>,
    pub scaling: Option<ScalingConfig>,
    pub prometheus: Option<PrometheusConfig>,
}

#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Clone, Debug)]
pub struct ScalingConfig {
    pub enabled: bool,
    pub channel: String,
    pub redis: RedisConfig,
}

#[derive(Clone, Debug)]
pub struct RedisConfig {
    pub url: Option<String>,
    pub host: String,
    pub port: u16,
    pub db: u8,
    pub username: Option<String>,
    pub password: Option<String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct PrometheusConfig {
    pub listen: String,
    pub bearer_token: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    apps: Vec<RawApp>,
}

#[derive(Deserialize, Serialize, Debug, Default)]
struct RawServer {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    path: String,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default = "default_max_request_size")]
    max_request_size: u64,
    #[serde(default = "default_restart_signal_file")]
    restart_signal_file: String,
    #[serde(default = "default_restart_poll")]
    restart_poll_interval_seconds: u64,
    #[serde(default)]
    tls: Option<RawTls>,
    #[serde(default)]
    scaling: Option<RawScaling>,
    #[serde(default)]
    prometheus: Option<RawPrometheus>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawTls {
    cert: String,
    key: String,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawScaling {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_scaling_channel")]
    channel: String,
    #[serde(default)]
    redis: RawRedis,
}

#[derive(Deserialize, Serialize, Debug, Default)]
struct RawRedis {
    url: Option<String>,
    #[serde(default = "default_redis_host")]
    host: String,
    #[serde(default = "default_redis_port")]
    port: u16,
    #[serde(default)]
    db: u8,
    username: Option<String>,
    password: Option<String>,
    #[serde(default = "default_redis_timeout")]
    timeout_seconds: u64,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawPrometheus {
    #[serde(default = "default_prometheus_listen")]
    listen: String,
    #[serde(default)]
    bearer_token: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawApp {
    id: String,
    key: String,
    secret: String,
    #[serde(default = "default_ping_interval")]
    ping_interval: u32,
    #[serde(default = "default_activity_timeout")]
    activity_timeout: u32,
    #[serde(default = "default_max_message_size")]
    max_message_size: u32,
    #[serde(default)]
    max_connections: Option<u32>,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    accept_client_events_from: AcceptClientEventsFrom,
    #[serde(default)]
    rate_limiting: Option<RateLimitConfig>,
    #[serde(default)]
    webhooks: Vec<Value>,
    #[serde(default)]
    emit_subscription_count: bool,
    #[serde(default)]
    encryption_master_key: Option<String>,
    /// Cache-channel TTL in seconds. Zero keeps payloads forever.
    #[serde(default = "default_cache_ttl_seconds")]
    cache_ttl_seconds: u64,
}

fn default_cache_ttl_seconds() -> u64 {
    1800
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8080
}
fn default_max_request_size() -> u64 {
    10_000
}
fn default_restart_signal_file() -> String {
    "/tmp/zatat.restart".into()
}
fn default_restart_poll() -> u64 {
    5
}
fn default_scaling_channel() -> String {
    "zatat".into()
}
fn default_redis_host() -> String {
    "127.0.0.1".into()
}
fn default_redis_port() -> u16 {
    6379
}
fn default_redis_timeout() -> u64 {
    60
}
fn default_prometheus_listen() -> String {
    "127.0.0.1:9090".into()
}
fn default_ping_interval() -> u32 {
    30
}
fn default_activity_timeout() -> u32 {
    30
}
fn default_max_message_size() -> u32 {
    10_000
}

impl RawConfig {
    fn compile(self) -> Result<Config, ConfigError> {
        let server = ServerConfig {
            host: self.server.host,
            port: self.server.port,
            path: self.server.path,
            hostname: self.server.hostname,
            max_request_size: self.server.max_request_size,
            restart_signal_file: self.server.restart_signal_file,
            restart_poll_interval: Duration::from_secs(self.server.restart_poll_interval_seconds),
            tls: self.server.tls.map(|t| TlsConfig {
                cert: t.cert,
                key: t.key,
            }),
            scaling: self.server.scaling.map(|s| ScalingConfig {
                enabled: s.enabled,
                channel: s.channel,
                redis: RedisConfig {
                    url: s.redis.url,
                    host: s.redis.host,
                    port: s.redis.port,
                    db: s.redis.db,
                    username: s.redis.username,
                    password: s.redis.password,
                    timeout_seconds: s.redis.timeout_seconds,
                },
            }),
            prometheus: self.server.prometheus.map(|p| PrometheusConfig {
                listen: p.listen,
                bearer_token: p.bearer_token,
            }),
        };

        let index = RawConfig {
            server: Default::default(),
            apps: self.apps,
        }
        .build_app_index()?;

        Ok(Config {
            server,
            apps: std::sync::Arc::new(arc_swap::ArcSwap::from(std::sync::Arc::new(index))),
        })
    }

    fn build_app_index(self) -> Result<AppIndex, ConfigError> {
        let mut by_id: HashMap<AppId, AppArc> = HashMap::new();
        let mut by_key: HashMap<AppKey, AppArc> = HashMap::new();
        for raw in self.apps {
            let app = Application::new(
                AppId::from(raw.id.clone()),
                AppKey::from(raw.key.clone()),
                raw.secret,
                raw.ping_interval,
                raw.activity_timeout,
                raw.max_message_size,
                raw.max_connections,
                raw.accept_client_events_from,
                raw.rate_limiting,
                raw.allowed_origins,
            )?
            .with_webhooks(raw.webhooks)
            .with_subscription_count(raw.emit_subscription_count)
            .with_encryption_master_key(raw.encryption_master_key)
            .with_cache_ttl_seconds(if raw.cache_ttl_seconds == 0 {
                None
            } else {
                Some(raw.cache_ttl_seconds)
            });
            let arc = Arc::new(app);
            if by_id.insert(arc.id.clone(), arc.clone()).is_some() {
                return Err(ConfigError::DuplicateAppId(raw.id));
            }
            if by_key.insert(arc.key.clone(), arc.clone()).is_some() {
                return Err(ConfigError::DuplicateAppKey(raw.key));
            }
        }
        Ok(AppIndex { by_id, by_key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_minimal_toml() {
        let tmp = std::env::temp_dir().join("zatat-test.toml");
        std::fs::write(
            &tmp,
            r#"
[server]
host = "127.0.0.1"
port = 8080

[[apps]]
id = "a"
key = "k"
secret = "s"
allowed_origins = ["*"]
"#,
        )
        .unwrap();
        let cfg = Config::load(&tmp).unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.apps().by_id.len(), 1);
        assert!(cfg.app_by_key(&AppKey::from("k")).is_some());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn reload_swaps_apps_atomically() {
        let tmp = std::env::temp_dir().join("zatat-reload-test.toml");
        std::fs::write(
            &tmp,
            r#"
[server]
host = "127.0.0.1"
port = 8080

[[apps]]
id = "a"
key = "k1"
secret = "s1"
allowed_origins = ["*"]
"#,
        )
        .unwrap();

        let cfg = Config::load(&tmp).unwrap();
        let before = cfg.app_by_key(&AppKey::from("k1")).unwrap();
        assert_eq!(before.secret, "s1");

        // Rewrite the file with rotated secret + extra app, reload.
        std::fs::write(
            &tmp,
            r#"
[server]
host = "127.0.0.1"
port = 8080

[[apps]]
id = "a"
key = "k1"
secret = "s1-new"
allowed_origins = ["*"]

[[apps]]
id = "b"
key = "k2"
secret = "s2"
allowed_origins = ["*"]
"#,
        )
        .unwrap();

        let n = cfg.reload_apps_from(&tmp).unwrap();
        assert_eq!(n, 2);

        let updated = cfg.app_by_key(&AppKey::from("k1")).unwrap();
        assert_eq!(updated.secret, "s1-new");
        assert!(cfg.app_by_key(&AppKey::from("k2")).is_some());

        // The Arc captured BEFORE reload keeps its old secret — existing
        // connections observe the config they opened with.
        assert_eq!(before.secret, "s1");

        let _ = std::fs::remove_file(&tmp);
    }
}
