use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::id::{AppId, AppKey};

pub type AppArc = Arc<Application>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcceptClientEventsFrom {
    All,
    #[default]
    Members,
    None,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    pub max_attempts: u32,
    pub decay_seconds: u32,
    #[serde(default)]
    pub terminate_on_limit: bool,
}

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("invalid origin pattern `{0}`: {1}")]
    InvalidOriginPattern(String, globset::Error),
}

pub struct Application {
    pub id: AppId,
    pub key: AppKey,
    pub secret: String,
    pub ping_interval: u32,
    pub activity_timeout: u32,
    pub max_message_size: u32,
    pub max_connections: Option<u32>,
    pub accept_client_events_from: AcceptClientEventsFrom,
    pub rate_limiting: Option<RateLimitConfig>,
    pub allowed_origins_raw: Vec<String>,
    allowed_origins: GlobSet,
    pub webhooks: Vec<Value>,
    pub emit_subscription_count: bool,
    pub encryption_master_key: Option<String>,
    /// Cache-channel TTL. `None` keeps payloads forever.
    pub cache_ttl_seconds: Option<u64>,
}

impl std::fmt::Debug for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Application")
            .field("id", &self.id)
            .field("key", &self.key)
            .field("ping_interval", &self.ping_interval)
            .field("activity_timeout", &self.activity_timeout)
            .field("max_message_size", &self.max_message_size)
            .field("max_connections", &self.max_connections)
            .field("accept_client_events_from", &self.accept_client_events_from)
            .field("allowed_origins", &self.allowed_origins_raw)
            .finish_non_exhaustive()
    }
}

impl Application {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: AppId,
        key: AppKey,
        secret: String,
        ping_interval: u32,
        activity_timeout: u32,
        max_message_size: u32,
        max_connections: Option<u32>,
        accept_client_events_from: AcceptClientEventsFrom,
        rate_limiting: Option<RateLimitConfig>,
        allowed_origins: Vec<String>,
    ) -> Result<Self, ApplicationError> {
        let globs = build_origin_globset(&allowed_origins)?;
        Ok(Self {
            id,
            key,
            secret,
            ping_interval,
            activity_timeout,
            max_message_size,
            max_connections,
            accept_client_events_from,
            rate_limiting,
            allowed_origins_raw: allowed_origins,
            allowed_origins: globs,
            webhooks: Vec::new(),
            emit_subscription_count: false,
            encryption_master_key: None,
            cache_ttl_seconds: None,
        })
    }

    pub fn with_cache_ttl_seconds(mut self, ttl: Option<u64>) -> Self {
        self.cache_ttl_seconds = ttl;
        self
    }

    pub fn with_webhooks(mut self, webhooks: Vec<Value>) -> Self {
        self.webhooks = webhooks;
        self
    }

    pub fn with_subscription_count(mut self, enabled: bool) -> Self {
        self.emit_subscription_count = enabled;
        self
    }

    pub fn with_encryption_master_key(mut self, key: Option<String>) -> Self {
        self.encryption_master_key = key;
        self
    }

    pub fn origin_is_allowed(&self, host: &str) -> bool {
        if self.allowed_origins_raw.iter().any(|p| p == "*") {
            return true;
        }
        self.allowed_origins.is_match(host)
    }
}

fn build_origin_globset(patterns: &[String]) -> Result<GlobSet, ApplicationError> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if p == "*" {
            continue;
        }
        let glob =
            Glob::new(p).map_err(|e| ApplicationError::InvalidOriginPattern(p.clone(), e))?;
        b.add(glob);
    }
    b.build()
        .map_err(|e| ApplicationError::InvalidOriginPattern("<combined>".into(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(origins: Vec<&str>) -> Application {
        Application::new(
            AppId::from("x"),
            AppKey::from("k"),
            "s".into(),
            30,
            30,
            10_000,
            None,
            AcceptClientEventsFrom::Members,
            None,
            origins.into_iter().map(|s| s.to_string()).collect(),
        )
        .unwrap()
    }

    #[test]
    fn origin_star_allows_anything() {
        let a = app(vec!["*"]);
        assert!(a.origin_is_allowed("anything"));
    }

    #[test]
    fn origin_exact_host() {
        let a = app(vec!["example.com"]);
        assert!(a.origin_is_allowed("example.com"));
        assert!(!a.origin_is_allowed("other.com"));
    }

    #[test]
    fn origin_glob_subdomain() {
        let a = app(vec!["*.example.com"]);
        assert!(a.origin_is_allowed("app.example.com"));
        assert!(!a.origin_is_allowed("example.com"));
        assert!(!a.origin_is_allowed("evil.com"));
    }
}
