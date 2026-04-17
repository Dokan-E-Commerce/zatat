use std::borrow::Borrow;
use std::fmt;

use rand::Rng;
use serde::{Deserialize, Serialize};

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(AppId);
string_newtype!(AppKey);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SocketId(String);

impl SocketId {
    /// Generates a `"{rand}.{rand}"` socket id where each half is in
    /// `1..=1_000_000_000`. No zero padding.
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let a: u64 = rng.random_range(1..=1_000_000_000);
        let b: u64 = rng.random_range(1..=1_000_000_000);
        Self(format!("{a}.{b}"))
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for SocketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for SocketId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_id_has_two_dot_separated_numeric_halves() {
        for _ in 0..32 {
            let id = SocketId::generate();
            let parts: Vec<&str> = id.as_str().split('.').collect();
            assert_eq!(parts.len(), 2, "expected `x.y`, got {}", id);
            let a: u64 = parts[0].parse().unwrap();
            let b: u64 = parts[1].parse().unwrap();
            assert!((1..=1_000_000_000).contains(&a));
            assert!((1..=1_000_000_000).contains(&b));
            assert_eq!(parts[0], a.to_string());
            assert_eq!(parts[1], b.to_string());
        }
    }
}
