//! Stable identifiers.
//!
//! All identifiers are 128-bit UUIDs rendered as lowercase hyphenated text.
//!
//! - **Random/time-ordered IDs** (events, spans, attempts, work units, ...)
//!   are UUIDv7 so they sort by creation time and remain globally unique
//!   without coordination.
//! - **Derived IDs** (canonical session id from a provider session id, project
//!   id from a normalised root path) are UUIDv5 under a fixed AttemptDB
//!   namespace so that two devices, or a re-import of the same spool, derive
//!   the same identifier deterministically.
//!
//! Provider-native identifiers are always stored *alongside* canonical ones,
//! never replaced by them.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Root namespace for all UUIDv5 derivations. Generated once; never change.
pub const ATTEMPTDB_NAMESPACE: Uuid = Uuid::from_bytes([
    0x3b, 0x9a, 0xc0, 0x8e, 0x7f, 0x21, 0x4b, 0x1d, 0x8e, 0x5a, 0x1c, 0x2d, 0x9f, 0x6e, 0x4a, 0x77,
]);

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Short human prefix used in CLI output (e.g. `ev_`, `ses_`).
            pub const PREFIX: &'static str = $prefix;

            /// Time-ordered random identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Deterministic identifier derived from `parts` under the
            /// AttemptDB namespace and this type's prefix.
            pub fn derive(parts: &[&str]) -> Self {
                let mut name = String::from($prefix);
                for p in parts {
                    name.push('\u{1f}');
                    name.push_str(p);
                }
                Self(Uuid::new_v5(&ATTEMPTDB_NAMESPACE, name.as_bytes()))
            }

            pub const fn nil() -> Self {
                Self(Uuid::nil())
            }

            pub fn is_nil(&self) -> bool {
                self.0.is_nil()
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            pub fn from_bytes(b: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(b))
            }

            /// Short display form: prefix plus the first 8 hex characters.
            pub fn short(&self) -> String {
                let s = self.0.simple().to_string();
                format!("{}{}", $prefix, &s[..8])
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::nil()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = crate::CoreError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let s = s.trim();
                let s = s.strip_prefix($prefix).unwrap_or(s);
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|e| crate::CoreError::InvalidId(format!("{s}: {e}")))
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }
    };
}

id_type!(
    /// Identifies one observed fact.
    EventId,
    "ev_"
);
id_type!(
    /// Identifies a device (machine + OS user). Generated once per data
    /// directory and persisted.
    DeviceId,
    "dev_"
);
id_type!(
    /// Identifies a project (normally a repository root). Derived from the
    /// repository remote when available, otherwise from the normalised root
    /// path plus device id.
    ProjectId,
    "prj_"
);
id_type!(
    /// Canonical session id. Derived from `(provider, provider_session_id)`.
    SessionId,
    "ses_"
);
id_type!(
    /// A turn: one human prompt and everything the agent did in response.
    TurnId,
    "trn_"
);
id_type!(
    /// A bounded execution range with causal parents and children.
    SpanId,
    "spn_"
);
id_type!(
    /// An agent or subagent instance within a session.
    AgentId,
    "agt_"
);
id_type!(
    /// One approach toward an objective, regardless of outcome.
    AttemptId,
    "att_"
);
id_type!(
    /// A versioned inferred unit of project work.
    WorkUnitId,
    "wu_"
);
id_type!(
    /// A recorded decision with alternatives and evidence.
    DecisionId,
    "dec_"
);
id_type!(
    /// A produced artifact: file, commit, PR, test result, document, URL.
    ArtifactId,
    "art_"
);
id_type!(
    /// An inference version.
    InferenceId,
    "inf_"
);
id_type!(
    /// A human correction event.
    CorrectionId,
    "cor_"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_deterministic_and_type_scoped() {
        let a = SessionId::derive(&["claude_code", "abc"]);
        let b = SessionId::derive(&["claude_code", "abc"]);
        let c = ProjectId::derive(&["claude_code", "abc"]);
        assert_eq!(a, b);
        assert_ne!(a.0, c.0);
        assert_ne!(
            SessionId::derive(&["claude_code", "ab"]),
            SessionId::derive(&["claude_cod", "eab"])
        );
    }

    #[test]
    fn v7_ids_sort_by_time() {
        let a = EventId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = EventId::new();
        assert!(a < b);
    }

    #[test]
    fn parse_accepts_prefix() {
        let id = EventId::new();
        let s = format!("ev_{id}");
        assert_eq!(s.parse::<EventId>().unwrap(), id);
        assert_eq!(id.to_string().parse::<EventId>().unwrap(), id);
    }
}

id_type!(
    /// A commit linked to the `git commit` tool call that produced it.
    CommitId,
    "cmt_"
);
id_type!(
    /// Two work units editing the same files at the same time (Tier 1
    /// `conflict-v0`), derived from the pair of work unit ids.
    ConflictId,
    "cfl_"
);
