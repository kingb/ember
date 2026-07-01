//! Identity newtypes for the multiplexer (design §5). Pure, serde-ready.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

/// A session reference (design §4). The full `SessionBackend` contract is Epic B;
/// here it is only the leaf's opaque handle.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<u64> for PaneId {
    fn from(n: u64) -> Self {
        Self(n)
    }
}

impl From<u64> for TabId {
    fn from(n: u64) -> Self {
        Self(n)
    }
}
