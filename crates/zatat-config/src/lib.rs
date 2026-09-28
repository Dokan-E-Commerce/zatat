#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use figment::providers::{Format, Toml};
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
    #[error("ZATAT_APPS__{0}__* overrides app index {0}, but only {1} app(s) precede it")]
    AppEnvIndexGap(usize, usize),
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

/// Result of re-reading the apps table.
#[derive(Debug, Default)]
pub struct AppsReload {
    /// Number of apps now active.
    pub loaded: usize,
    /// Apps that were removed or whose key changed. Their connections were
    /// opened under an identity that no longer exists and must be closed.
    pub revoked: Vec<AppId>,
    /// Apps whose allowed origins changed. Connections from origins that
    /// are no longer allowed must be closed.
    pub origins_changed: Vec<AppId>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        load_raw(Some(path))?.compile()
    }

    pub fn load_from_env() -> Result<Self, ConfigError> {
        load_raw(None)?.compile()
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

    /// Re-read `path` plus the `ZATAT_*` environment (same precedence as
    /// startup) and atomically replace the apps table. The `server` block is
    /// intentionally NOT re-applied — host/port/TLS/Redis changes still need
    /// a process restart.
    pub fn reload_apps_from(&self, path: &Path) -> Result<AppsReload, ConfigError> {
        self.reload_apps_with(path, std::env::vars())
    }

    fn reload_apps_with(
        &self,
        path: &Path,
        vars: impl Iterator<Item = (String, String)>,
    ) -> Result<AppsReload, ConfigError> {
        let next = load_raw_with(Some(path), vars)?.build_app_index()?;
        let previous = self.apps.load_full();
        let mut reload = AppsReload {
            loaded: next.by_id.len(),
            ..AppsReload::default()
        };
        for (id, old) in &previous.by_id {
            match next.by_id.get(id) {
                None => reload.revoked.push(id.clone()),
                Some(new) if new.key != old.key => reload.revoked.push(id.clone()),
                Some(new) if new.allowed_origins_raw != old.allowed_origins_raw => {
                    reload.origins_changed.push(id.clone())
                }
                Some(_) => {}
            }
        }
        self.apps.store(std::sync::Arc::new(next));
        Ok(reload)
    }
}

const ENV_PREFIX: &str = "ZATAT_";
const APP_ENV_PREFIX: &str = "ZATAT_APPS__";

/// TOML (when given) overlaid with `ZATAT_*` variables. Per-app overrides
/// (`ZATAT_APPS__<index>__<FIELD>`) are applied to the indexed `[[apps]]`
/// entry; figment alone would merge them as a map over the array and fail.
fn load_raw(path: Option<&Path>) -> Result<RawConfig, ConfigError> {
    load_raw_with(path, std::env::vars())
}

fn load_raw_with(
    path: Option<&Path>,
    vars: impl Iterator<Item = (String, String)>,
) -> Result<RawConfig, ConfigError> {
    let mut figment = Figment::new();
    if let Some(path) = path {
        figment = figment.merge(Toml::file(path));
    }
    let vars: Vec<(String, String)> = vars.collect();
    // `ZATAT_A__B=v` sets `a.b`. Values are parsed as TOML scalars (so
    // numbers, booleans and arrays work), except text fields, which keep the
    // raw value: parsing would turn a secret like `0042` into the number 42.
    for (name, value) in &vars {
        let Some(path) = env_path(name).filter(|p| !p.is_empty()) else {
            continue;
        };
        if path.starts_with("apps.") && app_env_index(&name[ENV_PREFIX.len()..]).is_some() {
            continue; // applied per app below
        }
        let value: figment::value::Value = if SERVER_TEXT_FIELDS.contains(&path.as_str()) {
            figment::value::Value::from(value.clone())
        } else {
            value.parse().expect("infallible")
        };
        figment = figment.merge(figment::providers::Serialized::defaults(
            figment::util::nest(&path, value),
        ));
    }
    let mut raw: RawConfig = figment.extract()?;
    raw.apps = apply_app_env_overrides(raw.apps, vars.into_iter())?;
    Ok(raw)
}

/// Server settings whose values are text even when they look numeric.
const SERVER_TEXT_FIELDS: &[&str] = &[
    "server.host",
    "server.path",
    "server.restart_signal_file",
    "server.tls.cert",
    "server.tls.key",
    "server.scaling.channel",
    "server.scaling.redis.url",
    "server.scaling.redis.host",
    "server.scaling.redis.username",
    "server.scaling.redis.password",
    "server.prometheus.listen",
    "server.prometheus.bearer_token",
];

/// App settings whose values are text even when they look numeric.
const APP_TEXT_FIELDS: &[&str] = &["id", "key", "secret", "encryption_master_key"];

/// `ZATAT_SERVER__SCALING__PORT` → `server.scaling.port`.
fn env_path(name: &str) -> Option<String> {
    let key = name
        .get(..ENV_PREFIX.len())
        .filter(|p| p.eq_ignore_ascii_case(ENV_PREFIX))
        .map(|_| &name[ENV_PREFIX.len()..])?;
    Some(key.to_ascii_lowercase().replace("__", "."))
}

/// `apps__3__secret` → `Some(3)`. Keys arrive with the prefix stripped.
fn app_env_index(key: &str) -> Option<usize> {
    let rest = key
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("apps__"))
        .map(|_| &key[6..])?;
    let (index, field) = rest.split_once("__")?;
    if field.is_empty() {
        return None;
    }
    index.parse().ok()
}

fn apply_app_env_overrides(
    mut apps: Vec<RawApp>,
    vars: impl Iterator<Item = (String, String)>,
) -> Result<Vec<RawApp>, ConfigError> {
    let mut overrides: std::collections::BTreeMap<usize, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (name, value) in vars {
        let Some(key) = name
            .get(..ENV_PREFIX.len())
            .filter(|p| p.eq_ignore_ascii_case(ENV_PREFIX))
            .map(|_| &name[ENV_PREFIX.len()..])
        else {
            continue;
        };
        if let Some(index) = app_env_index(key) {
            let field = key[APP_ENV_PREFIX.len() - ENV_PREFIX.len()..]
                .split_once("__")
                .map(|(_, f)| f.to_string())
                .unwrap_or_default();
            overrides.entry(index).or_default().push((field, value));
        }
    }
    for (index, fields) in overrides {
        if index > apps.len() {
            return Err(ConfigError::AppEnvIndexGap(index, apps.len()));
        }
        let mut figment = match apps.get(index) {
            Some(app) => Figment::from(figment::providers::Serialized::defaults(app)),
            None => Figment::new(),
        };
        for (field, value) in fields {
            let path = field.to_ascii_lowercase().replace("__", ".");
            let value: figment::value::Value = if APP_TEXT_FIELDS.contains(&path.as_str()) {
                figment::value::Value::from(value)
            } else {
                value.parse().expect("infallible")
            };
            figment = figment.merge(figment::providers::Serialized::defaults(
                figment::util::nest(&path, value),
            ));
        }
        let app: RawApp = figment.extract()?;
        if index == apps.len() {
            apps.push(app);
        } else {
            apps[index] = app;
        }
    }
    Ok(apps)
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub max_request_size: u64,
    pub restart_signal_file: String,
    pub restart_poll_interval: Duration,
    pub tls: Option<TlsConfig>,
    pub scaling: Option<ScalingConfig>,
    pub prometheus: Option<PrometheusConfig>,
    /// `"best_effort"` (default, drop on full) or `"block"` (bounded
    /// staging buffer, then drop). Controls how the webhook dispatcher
    /// reacts when its in-memory queue fills.
    pub webhook_overflow_mode: OverflowMode,
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
    /// How the outbound publisher reacts when the in-memory queue is full.
    /// `BestEffort` (default) drops the payload + counts it via
    /// `zatat_scaling_publish_drops_total`. `Block` first overflows into a
    /// byte-bounded ordered staging buffer; callers never wait.
    pub overflow_mode: OverflowMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OverflowMode {
    /// Drop excess work + count it; never blocks the producer. Default,
    /// safe for every workload.
    #[default]
    BestEffort,
    /// Absorb overload in a byte-bounded ordered staging buffer before
    /// dropping (and counting). The producer never waits; this trades
    /// memory for riding out longer consumer stalls. Not durable.
    Block,
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

/// Environment values are parsed as TOML scalars, so `ZATAT_APPS__0__ID=123`
/// arrives as an integer and an all-digit secret as a number. String fields
/// accept any scalar and keep its text.
struct ScalarString(String);

impl<'de> Deserialize<'de> for ScalarString {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = ScalarString;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string or scalar")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<ScalarString, E> {
                Ok(ScalarString(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_i128<E: serde::de::Error>(self, v: i128) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
            fn visit_char<E: serde::de::Error>(self, v: char) -> Result<ScalarString, E> {
                Ok(ScalarString(v.to_string()))
            }
        }
        d.deserialize_any(Visitor)
    }
}

fn scalar_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    ScalarString::deserialize(d).map(|s| s.0)
}

fn opt_scalar_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<ScalarString>::deserialize(d).map(|o| o.map(|s| s.0))
}

#[derive(Deserialize, Serialize, Debug)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    apps: Vec<RawApp>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawServer {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    path: String,
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
    #[serde(default)]
    webhook_overflow_mode: RawOverflowMode,
}

impl Default for RawServer {
    fn default() -> Self {
        RawServer {
            host: default_host(),
            port: default_port(),
            path: String::default(),
            max_request_size: default_max_request_size(),
            restart_signal_file: default_restart_signal_file(),
            restart_poll_interval_seconds: default_restart_poll(),
            tls: None,
            scaling: None,
            prometheus: None,
            webhook_overflow_mode: RawOverflowMode::default(),
        }
    }
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
    /// `"best_effort"` (default, drops on full) or `"block"` (bounded staging).
    #[serde(default)]
    overflow_mode: RawOverflowMode,
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
enum RawOverflowMode {
    #[default]
    BestEffort,
    Block,
}

impl From<RawOverflowMode> for OverflowMode {
    fn from(r: RawOverflowMode) -> Self {
        match r {
            RawOverflowMode::BestEffort => OverflowMode::BestEffort,
            RawOverflowMode::Block => OverflowMode::Block,
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct RawRedis {
    #[serde(default, deserialize_with = "opt_scalar_string")]
    url: Option<String>,
    #[serde(default = "default_redis_host")]
    host: String,
    #[serde(default = "default_redis_port")]
    port: u16,
    #[serde(default)]
    db: u8,
    #[serde(default, deserialize_with = "opt_scalar_string")]
    username: Option<String>,
    #[serde(default, deserialize_with = "opt_scalar_string")]
    password: Option<String>,
    #[serde(default = "default_redis_timeout")]
    timeout_seconds: u64,
}

impl Default for RawRedis {
    fn default() -> Self {
        RawRedis {
            url: None,
            host: default_redis_host(),
            port: default_redis_port(),
            db: u8::default(),
            username: None,
            password: None,
            timeout_seconds: default_redis_timeout(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct RawPrometheus {
    #[serde(default = "default_prometheus_listen")]
    listen: String,
    #[serde(default, deserialize_with = "opt_scalar_string")]
    bearer_token: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RawApp {
    #[serde(deserialize_with = "scalar_string")]
    id: String,
    #[serde(deserialize_with = "scalar_string")]
    key: String,
    #[serde(deserialize_with = "scalar_string")]
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
    #[serde(default, deserialize_with = "opt_scalar_string")]
    encryption_master_key: Option<String>,
    /// Cache-channel TTL in seconds. Zero keeps payloads forever.
    #[serde(default = "default_cache_ttl_seconds")]
    cache_ttl_seconds: u64,
    #[serde(default = "default_max_presence_members")]
    max_presence_members_per_channel: u32,
    #[serde(default = "default_max_presence_member_size_bytes")]
    max_presence_member_size_bytes: u32,
    #[serde(default = "zatat_core::application::default_max_channels_per_connection")]
    max_channels_per_connection: u32,
}

fn default_cache_ttl_seconds() -> u64 {
    1800
}
fn default_max_presence_members() -> u32 {
    100
}
fn default_max_presence_member_size_bytes() -> u32 {
    2048
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
                overflow_mode: s.overflow_mode.into(),
            }),
            prometheus: self.server.prometheus.map(|p| PrometheusConfig {
                listen: p.listen,
                bearer_token: p.bearer_token,
            }),
            webhook_overflow_mode: self.server.webhook_overflow_mode.into(),
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
            if raw.accept_client_events_from == AcceptClientEventsFrom::All {
                tracing::warn!(
                    app = %raw.id,
                    "accept_client_events_from = \"all\" is accepted for compatibility but behaves as \"members\" — channel membership is always required"
                );
            }
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
            })
            .with_presence_limits(
                raw.max_presence_members_per_channel,
                raw.max_presence_member_size_bytes,
            )
            .with_max_channels_per_connection(raw.max_channels_per_connection);
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
        let app = cfg.app_by_key(&AppKey::from("k")).unwrap();
        assert_eq!(app.max_presence_members_per_channel, 100);
        assert_eq!(app.max_presence_member_size_bytes, 2048);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn presence_limits_can_be_overridden() {
        let tmp = std::env::temp_dir().join("zatat-presence-limits-test.toml");
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
max_presence_members_per_channel = 5
max_presence_member_size_bytes = 128
"#,
        )
        .unwrap();
        let cfg = Config::load(&tmp).unwrap();
        let app = cfg.app_by_key(&AppKey::from("k")).unwrap();
        assert_eq!(app.max_presence_members_per_channel, 5);
        assert_eq!(app.max_presence_member_size_bytes, 128);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn missing_server_section_uses_field_defaults() {
        let tmp = std::env::temp_dir().join("zatat-no-server-section-test.toml");
        std::fs::write(
            &tmp,
            r#"
[[apps]]
id = "a"
key = "k"
secret = "s"
allowed_origins = ["*"]
"#,
        )
        .unwrap();
        let cfg = Config::load(&tmp).unwrap();
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.port, 8080);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn scaling_without_redis_subsection_uses_redis_defaults() {
        let tmp = std::env::temp_dir().join("zatat-scaling-no-redis-test.toml");
        std::fs::write(
            &tmp,
            r#"
[server]
host = "127.0.0.1"
port = 8080

[server.scaling]
enabled = false

[[apps]]
id = "a"
key = "k"
secret = "s"
allowed_origins = ["*"]
"#,
        )
        .unwrap();
        let cfg = Config::load(&tmp).unwrap();
        let scaling = cfg.server.scaling.as_ref().unwrap();
        assert!(!scaling.enabled);
        assert_eq!(scaling.redis.host, "127.0.0.1");
        assert_eq!(scaling.redis.port, 6379);
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

        let reload = cfg.reload_apps_from(&tmp).unwrap();
        assert_eq!(reload.loaded, 2);
        // A secret rotation keeps connections; they read the live app.
        assert!(reload.revoked.is_empty());

        let updated = cfg.app_by_key(&AppKey::from("k1")).unwrap();
        assert_eq!(updated.secret, "s1-new");
        assert!(cfg.app_by_key(&AppKey::from("k2")).is_some());

        // The Arc captured BEFORE reload keeps its old secret — existing
        // connections observe the config they opened with.
        assert_eq!(before.secret, "s1");

        let _ = std::fs::remove_file(&tmp);
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Iterator<Item = (String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn write_single_app(name: &str, secret: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(name);
        std::fs::write(
            &tmp,
            format!(
                r#"
[[apps]]
id = "a"
key = "k"
secret = "{secret}"
allowed_origins = ["*"]
max_message_size = 4096
"#
            ),
        )
        .unwrap();
        tmp
    }

    #[test]
    fn indexed_app_env_override_replaces_one_field() {
        let tmp = write_single_app("zatat-env-override-test.toml", "from-file");
        let raw = load_raw_with(
            Some(&tmp),
            vars(&[
                ("ZATAT_APPS__0__SECRET", "from-env"),
                ("zatat_apps__0__rate_limiting__max_attempts", "7"),
                ("ZATAT_APPS__0__RATE_LIMITING__DECAY_SECONDS", "3"),
                ("ZATAT_APPS_RELOAD_INTERVAL_S", "1"),
            ]),
        )
        .unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(raw.apps.len(), 1);
        assert_eq!(raw.apps[0].secret, "from-env");
        assert_eq!(raw.apps[0].key, "k");
        assert_eq!(raw.apps[0].max_message_size, 4096);
        let rl = raw.apps[0].rate_limiting.unwrap();
        assert_eq!((rl.max_attempts, rl.decay_seconds), (7, 3));
    }

    #[test]
    fn indexed_app_env_override_can_append_an_app() {
        let tmp = write_single_app("zatat-env-append-test.toml", "s");
        let raw = load_raw_with(
            Some(&tmp),
            vars(&[
                ("ZATAT_APPS__1__ID", "b"),
                ("ZATAT_APPS__1__KEY", "kb"),
                ("ZATAT_APPS__1__SECRET", "sb"),
            ]),
        )
        .unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(raw.apps.len(), 2);
        assert_eq!(raw.apps[1].id, "b");
        let err = load_raw_with(None, vars(&[("ZATAT_APPS__2__ID", "c")])).unwrap_err();
        assert!(matches!(err, ConfigError::AppEnvIndexGap(2, 0)));
    }

    #[test]
    fn reload_keeps_env_overrides_and_reports_revocations() {
        let tmp = write_single_app("zatat-env-reload-test.toml", "from-file");
        let env = [("ZATAT_APPS__0__SECRET", "from-env")];
        let cfg = load_raw_with(Some(&tmp), vars(&env))
            .unwrap()
            .compile()
            .unwrap();
        assert_eq!(cfg.app_by_id(&AppId::from("a")).unwrap().secret, "from-env");

        // A touch with identical content must not fall back to the file secret
        // nor revoke live connections.
        let reload = cfg.reload_apps_with(&tmp, vars(&env)).unwrap();
        assert_eq!(cfg.app_by_id(&AppId::from("a")).unwrap().secret, "from-env");
        assert!(reload.revoked.is_empty());

        // Rotating the secret does not revoke connections (they read the live
        // app); changing the key or the allowed origins does.
        let reload = cfg
            .reload_apps_with(&tmp, vars(&[("ZATAT_APPS__0__SECRET", "rotated")]))
            .unwrap();
        assert!(reload.revoked.is_empty() && reload.origins_changed.is_empty());
        let reload = cfg
            .reload_apps_with(
                &tmp,
                vars(&[("ZATAT_APPS__0__ALLOWED_ORIGINS", "[\"a.example\"]")]),
            )
            .unwrap();
        assert_eq!(reload.origins_changed, vec![AppId::from("a")]);
        let reload = cfg
            .reload_apps_with(&tmp, vars(&[("ZATAT_APPS__0__KEY", "new-key")]))
            .unwrap();
        assert_eq!(reload.revoked, vec![AppId::from("a")]);

        // Removing the app also revokes it.
        std::fs::write(&tmp, "apps = []\n").unwrap();
        let reload = cfg.reload_apps_with(&tmp, vars(&[])).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(reload.loaded, 0);
        assert_eq!(reload.revoked, vec![AppId::from("a")]);
    }

    #[test]
    fn numeric_env_values_load_into_string_fields() {
        let raw = load_raw_with(
            None,
            vars(&[
                ("ZATAT_APPS__0__ID", "123456"),
                ("ZATAT_APPS__0__KEY", "true"),
                ("ZATAT_APPS__0__SECRET", "0042"),
            ]),
        )
        .unwrap();
        assert_eq!(raw.apps[0].id, "123456");
        assert_eq!(raw.apps[0].key, "true");
        // Text is kept verbatim: parsing would turn 0042 into 42.
        assert_eq!(raw.apps[0].secret, "0042");
    }

    #[test]
    fn server_text_env_values_are_kept_verbatim() {
        let raw = load_raw_with(
            None,
            vars(&[
                ("ZATAT_SERVER__SCALING__REDIS__PASSWORD", "000123"),
                ("ZATAT_SERVER__PORT", "9090"),
            ]),
        )
        .unwrap();
        assert_eq!(raw.server.port, 9090);
        let scaling = raw.server.scaling.expect("scaling section from env");
        assert_eq!(scaling.redis.password.as_deref(), Some("000123"));
    }

    #[test]
    fn unrelated_zatat_variables_are_ignored() {
        let raw = load_raw_with(
            None,
            vars(&[
                ("ZATAT_", "x"),
                ("ZATAT_CONFIG", "/etc/zatat/zatat.toml"),
                ("ZATAT_BIN", "/usr/local/bin/zatat"),
                ("ZATAT_APPS_RELOAD_INTERVAL_S", "5"),
                ("PATH", "/usr/bin"),
            ]),
        )
        .unwrap();
        assert!(raw.apps.is_empty());
        assert_eq!(raw.server.port, 8080);
    }

    #[test]
    fn app_env_index_parsing() {
        assert_eq!(app_env_index("apps__0__secret"), Some(0));
        assert_eq!(app_env_index("APPS__12__KEY"), Some(12));
        assert_eq!(app_env_index("apps__x__key"), None);
        assert_eq!(app_env_index("apps__0__"), None);
        assert_eq!(app_env_index("apps_reload_interval_s"), None);
        assert_eq!(app_env_index("server__port"), None);
    }
}
