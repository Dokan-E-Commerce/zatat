use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_CHANNEL_NAME_LEN: usize = 164;
pub const MAX_EVENT_NAME_LEN: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ChannelKind {
    Public,
    Private,
    Presence,
    Cache,
    PrivateCache,
    PresenceCache,
    PrivateEncrypted,
}

impl ChannelKind {
    pub fn from_name(name: &str) -> Self {
        if name.starts_with("private-encrypted-") {
            ChannelKind::PrivateEncrypted
        } else if name.starts_with("presence-cache-") {
            ChannelKind::PresenceCache
        } else if name.starts_with("private-cache-") {
            ChannelKind::PrivateCache
        } else if name.starts_with("cache-") {
            ChannelKind::Cache
        } else if name.starts_with("presence-") {
            ChannelKind::Presence
        } else if name.starts_with("private-") {
            ChannelKind::Private
        } else {
            ChannelKind::Public
        }
    }

    pub fn is_private(self) -> bool {
        matches!(
            self,
            ChannelKind::Private
                | ChannelKind::Presence
                | ChannelKind::PrivateCache
                | ChannelKind::PresenceCache
                | ChannelKind::PrivateEncrypted
        )
    }

    pub fn is_presence(self) -> bool {
        matches!(self, ChannelKind::Presence | ChannelKind::PresenceCache)
    }

    pub fn is_cache(self) -> bool {
        matches!(
            self,
            ChannelKind::Cache | ChannelKind::PrivateCache | ChannelKind::PresenceCache
        )
    }

    pub fn is_encrypted(self) -> bool {
        matches!(self, ChannelKind::PrivateEncrypted)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelName(String);

impl ChannelName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> ChannelKind {
        ChannelKind::from_name(&self.0)
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl From<String> for ChannelName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ChannelName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pusher_length_caps_are_the_documented_values() {
        assert_eq!(MAX_CHANNEL_NAME_LEN, 164);
        assert_eq!(MAX_EVENT_NAME_LEN, 200);
    }

    #[test]
    fn channel_kinds() {
        assert_eq!(ChannelKind::from_name("chat"), ChannelKind::Public);
        assert_eq!(ChannelKind::from_name("private-x"), ChannelKind::Private);
        assert_eq!(ChannelKind::from_name("presence-x"), ChannelKind::Presence);
        assert_eq!(ChannelKind::from_name("cache-x"), ChannelKind::Cache);
        assert_eq!(
            ChannelKind::from_name("private-cache-x"),
            ChannelKind::PrivateCache
        );
        assert_eq!(
            ChannelKind::from_name("presence-cache-x"),
            ChannelKind::PresenceCache
        );
        assert_eq!(
            ChannelKind::from_name("private-encrypted-x"),
            ChannelKind::PrivateEncrypted
        );

        assert!(ChannelKind::Presence.is_presence());
        assert!(ChannelKind::PresenceCache.is_presence());
        assert!(ChannelKind::PrivateEncrypted.is_private());
        assert!(ChannelKind::PrivateCache.is_cache());
    }
}
