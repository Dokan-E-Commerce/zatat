#![forbid(unsafe_code)]

mod channel;
mod manager;

pub use channel::{Channel, ChannelStats, Member, UnsubscribeOutcome};
pub use manager::{ChannelManager, ChannelManagerError, SubscribeOutcome};
