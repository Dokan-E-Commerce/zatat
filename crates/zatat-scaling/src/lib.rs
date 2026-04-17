#![forbid(unsafe_code)]

pub mod dispatcher;
pub mod message;
pub mod presence_cache;
pub mod provider;
pub mod redis;

pub use dispatcher::EventDispatcher;
pub use message::{
    PresenceSnapshotMember, ScalingEnvelope, ScalingPayload, SCALING_VERSION, SNAPSHOT_TTL,
};
pub use presence_cache::PresenceCache;
pub use provider::{LocalOnlyProvider, PubSubProvider};
pub use redis::RedisPubSubProvider;
