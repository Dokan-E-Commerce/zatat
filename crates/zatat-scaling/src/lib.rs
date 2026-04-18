#![forbid(unsafe_code)]

pub mod dispatcher;
pub mod message;
pub mod peer_state;
pub mod presence_cache;
pub mod provider;
pub mod redis;

pub use dispatcher::{EventDispatcher, PublishOverflow};
pub use message::{
    ChannelCount, PresenceSnapshotMember, ScalingEnvelope, ScalingPayload, SCALING_VERSION,
    SNAPSHOT_TTL,
};
pub use peer_state::{PeerChannelCounts, PeerUserSessions};
pub use presence_cache::PresenceCache;
pub use provider::{LocalOnlyProvider, PubSubProvider};
pub use redis::RedisPubSubProvider;
