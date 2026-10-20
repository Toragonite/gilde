//! Typed identifier newtypes.
//!
//! Every identifier is a UUID v7 string so that lexicographic order equals
//! creation order — age queries on `proc_registry` and dispatch history stay
//! cheap without a secondary index.
//!
//! The newtypes are `#[serde(transparent)]`: on the wire they are plain
//! strings, exactly as `src/lib/ipc.ts` declares them.

use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Prefix used when this id appears as a graph node id.
            pub const NODE_PREFIX: &'static str = $prefix;

            /// Generate a fresh, time-ordered identifier (UUID v7).
            pub fn new() -> Self {
                Self(Uuid::now_v7().to_string())
            }

            /// Wrap an existing identifier string (e.g. read back from SQLite).
            pub fn from_string(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }

            /// Render as a graph node id: `"<prefix>:<id>"`.
            pub fn node_id(&self) -> String {
                format!("{}:{}", $prefix, self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_owned()))
            }
        }
    };
}

define_id!(
    /// A project (a git repository the user added).
    ProjectId,
    "project"
);
define_id!(
    /// A git worktree owned or adopted by the app.
    WorktreeId,
    "worktree"
);
define_id!(
    /// An interactive PTY session.
    SessionId,
    "session"
);
define_id!(
    /// A dispatch task (one prompt fanned out to N agents).
    TaskId,
    "dispatch"
);
define_id!(
    /// One headless run inside a dispatch task.
    RunId,
    "run"
);
define_id!(
    /// An account slot (a provider config directory).
    AccountId,
    "account"
);
define_id!(
    /// A row in the durable PID registry. Written before the process exists.
    SpawnId,
    "spawn"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_time_ordered() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert!(a.as_str() < b.as_str(), "uuid v7 must be lexicographically ordered");
    }

    #[test]
    fn ids_serialize_transparently() {
        let id = RunId::from_string("0192f000-0000-7000-8000-000000000000");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"0192f000-0000-7000-8000-000000000000\"");
        let back: RunId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn node_id_uses_the_kind_prefix() {
        let id = WorktreeId::from_string("abc");
        assert_eq!(id.node_id(), "worktree:abc");
        assert_eq!(WorktreeId::NODE_PREFIX, "worktree");
        assert_eq!(TaskId::NODE_PREFIX, "dispatch");
    }
}
