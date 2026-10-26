//! Identifiers (contract §3.2). Replaces the old `model/ids.rs`.
//!
//! All ids are ASCII strings and never contain a path or user content (they
//! appear in logs). Ids are at most 96 bytes unless their row states otherwise
//! (`RequestId` 128, `BlockId` 160, `HistoryCursor` 256). Hash ids use the
//! first 16 lowercase hex chars of SHA-256 over the stated input.
//!
//! On the wire every id is a plain string (`#[serde(transparent)]`); the TS
//! generator brands them (`string & { readonly __brand: "SessionUid" }`).
//! Deserialization accepts any string — a non-matching id is a **value** error
//! (`INVALID_ARGUMENT{field, rule: "pattern"}`) reported by [`Validate`], so
//! the WebView learns which field was wrong (§4.1 rule 3 (b)).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

use crate::contract::common::{EpochMs, Vendor};
use crate::contract::validate::{Checked, Invalid, Validate};

/// Default byte cap of an id (§3.2).
pub const MAX_ID_BYTES: usize = 96;

// ---------------------------------------------------------------------------
// shape predicates
// ---------------------------------------------------------------------------

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `<prefix><n lowercase hex>`.
fn prefixed_hex(s: &str, prefix: &str, n: usize) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == n && is_lower_hex(rest))
}

/// Canonical lowercase UUID `8-4-4-4-12`; `version` = required version nibble
/// (`None` = any), and when a version is given the variant must be RFC 4122
/// (`[89ab]`).
pub fn is_uuid(s: &str, version: Option<u8>) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        let dash = matches!(i, 8 | 13 | 18 | 23);
        if dash != (c == b'-') {
            return false;
        }
        if !(dash || c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
            return false;
        }
    }
    match version {
        None => true,
        Some(v) => b[14] == b'0' + v && matches!(b[19], b'8' | b'9' | b'a' | b'b'),
    }
}

/// Printable ASCII (0x21..=0x7e plus space) of `min..=max` bytes.
fn printable_ascii(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// `^[a-z0-9-]{1,32}$`.
fn view_like(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `^x_[0-9]{1,10}_[0-9]{1,12}$`.
fn process_key(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("x_") else {
        return false;
    };
    let Some((pid, start)) = rest.split_once('_') else {
        return false;
    };
    (1..=10).contains(&pid.len())
        && (1..=12).contains(&start.len())
        && pid.bytes().all(|b| b.is_ascii_digit())
        && start.bytes().all(|b| b.is_ascii_digit())
}

/// `^%[0-9]{1,6}$`.
fn pane_id(s: &str) -> bool {
    s.strip_prefix('%')
        .is_some_and(|d| (1..=6).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()))
}

/// base64url (no padding requirement) of 1..=256 bytes.
fn base64url(s: &str) -> bool {
    (1..=256).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
}

/// First 16 lowercase hex chars of SHA-256 over `input`.
pub fn hash16(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    hex::encode(digest)[..16].to_string()
}

/// 16 random bytes as 32 lowercase hex chars (tokens).
fn random_hex32() -> String {
    hex::encode(uuid::Uuid::new_v4().into_bytes())
}

fn join_nul(parts: &[&str]) -> Vec<u8> {
    parts.join("\0").into_bytes()
}

// ---------------------------------------------------------------------------
// the newtypes
// ---------------------------------------------------------------------------

macro_rules! contract_id {
    ($(#[$meta:meta])* $name:ident, $check:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
        #[serde(transparent)]
        #[cfg_attr(feature = "ts-export", ts(export))]
        pub struct $name(String);

        impl $name {
            /// Whether `s` has this id's §3.2 format.
            pub fn is_valid_str(s: &str) -> bool {
                let check: fn(&str) -> bool = $check;
                check(s)
            }

            /// Accept `s` if it has this id's format (`rule: "pattern"` otherwise).
            pub fn parse(s: impl Into<String>) -> Result<$name, Invalid> {
                let s = s.into();
                if Self::is_valid_str(&s) {
                    Ok($name(s))
                } else {
                    Err(Invalid::new("", "pattern"))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl Validate for $name {
            fn validate(&self) -> Checked {
                if Self::is_valid_str(&self.0) {
                    Ok(())
                } else {
                    Err(Invalid::new("", "pattern"))
                }
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

contract_id!(
    /// `^p_[0-9a-f]{16}$` = sha256(realpath(gitCommonDir) or, for a non-repo
    /// folder, realpath(folder)). Stable across launches.
    ProjectId,
    |s| prefixed_hex(s, "p_", 16)
);
contract_id!(
    /// `^l_[0-9a-f]{16}$` = sha256(realpath(worktree root or folder)).
    LocationId,
    |s| prefixed_hex(s, "l_", 16)
);
contract_id!(
    /// `^a_[0-9a-f]{16}$` = sha256(vendor + "\0" + realpath(configDir)); the
    /// cursor/agy default login: sha256(vendor + "\0" + "default").
    AccountId,
    |s| prefixed_hex(s, "a_", 16)
);
contract_id!(
    /// `^s_<UUIDv7>$`, minted by World on first sight (§3.3).
    SessionUid,
    |s| s.strip_prefix("s_").is_some_and(|u| is_uuid(u, Some(7)))
);
contract_id!(
    /// `^t_[0-9a-f]{16}$` = sha256(source + "\0" + accountId + "\0" + nativeTeamId).
    TeamKey,
    |s| prefixed_hex(s, "t_", 16)
);
contract_id!(
    /// `^m_[0-9a-f]{16}$` = sha256(teamKey + "\0" + agentId + "\0" + joinedAt).
    MemberKey,
    |s| prefixed_hex(s, "m_", 16)
);
contract_id!(
    /// `^k_[0-9a-f]{16}$` = sha256(teamKey + "\0" + nativeTaskId); inferred items:
    /// sha256(teamKey + "\0inferred\0" + memberKey + "\0" + openingRowUuid).
    TaskKey,
    |s| prefixed_hex(s, "k_", 16)
);
contract_id!(
    /// `^g_[0-9a-f]{16}$` = sha256(teamKey + "\0" + (msg_id or from+to+at+ordinal)).
    MessageKey,
    |s| prefixed_hex(s, "g_", 16)
);
contract_id!(
    /// `^x_[0-9]{1,10}_[0-9]{1,12}$` = `x_<pid>_<kernel start seconds>`.
    ProcessKey,
    process_key
);
contract_id!(
    /// `^y_<UUIDv7>$`, minted by the PTY driver.
    PtyId,
    |s| s.strip_prefix("y_").is_some_and(|u| is_uuid(u, Some(7)))
);
contract_id!(
    /// `^%[0-9]{1,6}$`, per hosted lead, from 0 upward, never reused.
    PaneId,
    pane_id
);
contract_id!(
    /// Printable ASCII, 1–128 bytes: the vendor `request_id` (Conversation),
    /// `sp_` + `prompt_rev` (screen prompt), `wa_` + digest (watched approval).
    RequestId,
    |s| printable_ascii(s, 1, 128)
);
contract_id!(
    /// Lowercase UUIDv4, minted by the WebView for idempotency.
    ClientMsgId,
    |s| is_uuid(s, Some(4))
);
contract_id!(
    /// Lowercase UUIDv4, minted by the WebView for idempotency.
    ClientRequestId,
    |s| is_uuid(s, Some(4))
);
contract_id!(
    /// `^e_[0-9a-f]{16}$` = sha256(uid + "\0" + stateKind + "\0" + enteredAt).
    EpisodeId,
    |s| prefixed_hex(s, "e_", 16)
);
contract_id!(
    /// Printable ASCII ≤ 160 bytes: stream blocks `<message.id>#<index>`,
    /// transcript blocks the row `uuid` (§3.2, F-06).
    BlockId,
    |s| printable_ascii(s, 1, 160)
);
contract_id!(
    /// `^i_[0-9a-f]{16}$` = sha256(mediaType + "\0" + base64 data): an image a
    /// conversation block shows, held in memory by the core (§3.7).
    ImageId,
    |s| prefixed_hex(s, "i_", 16)
);
contract_id!(
    /// `^r_<UUIDv7>$`, minted when a quit or crash record is written.
    QuitRecordId,
    |s| s.strip_prefix("r_").is_some_and(|u| is_uuid(u, Some(7)))
);
contract_id!(
    /// `^c_[0-9a-f]{32}$`: 16 random bytes; single use; 60 s.
    ConfirmToken,
    |s| prefixed_hex(s, "c_", 32)
);
contract_id!(
    /// `^u_[0-9a-f]{32}$`: 16 random bytes; single use; 10 s.
    UndoToken,
    |s| prefixed_hex(s, "u_", 32)
);
contract_id!(
    /// `^v_[0-9a-f]{16}$`, minted by `convo_subscribe`.
    SubId,
    |s| prefixed_hex(s, "v_", 16)
);
contract_id!(
    /// `^[a-z0-9-]{1,32}$`, chosen by the WebView per view.
    ViewId,
    view_like
);
contract_id!(
    /// `^[a-z0-9-]{1,32}$`, chosen by the WebView per History query.
    QueryId,
    view_like
);
contract_id!(
    /// Opaque base64url ≤ 256 bytes, minted by `history_query`.
    HistoryCursor,
    base64url
);

// ---------------------------------------------------------------------------
// derivations (§3.2 "Derivation" column)
// ---------------------------------------------------------------------------

impl ProjectId {
    /// From `realpath(gitCommonDir)` (or the folder's realpath).
    pub fn derive(realpath: &str) -> ProjectId {
        ProjectId(format!("p_{}", hash16(realpath.as_bytes())))
    }
}

impl ImageId {
    /// sha256(mediaType + "\0" + base64 data): the same image is one id.
    pub fn derive(media_type: &str, data: &str) -> ImageId {
        ImageId(format!("i_{}", hash16(&join_nul(&[media_type, data]))))
    }
}

impl LocationId {
    /// From `realpath(worktree root or folder)`.
    pub fn derive(realpath: &str) -> LocationId {
        LocationId(format!("l_{}", hash16(realpath.as_bytes())))
    }
}

impl AccountId {
    /// sha256(vendor + "\0" + realpath(configDir)).
    pub fn derive(vendor: Vendor, config_dir_realpath: &str) -> AccountId {
        AccountId(format!(
            "a_{}",
            hash16(&join_nul(&[vendor.as_str(), config_dir_realpath]))
        ))
    }

    /// The cursor/agy default login: sha256(vendor + "\0" + "default").
    pub fn derive_default_login(vendor: Vendor) -> AccountId {
        AccountId(format!("a_{}", hash16(&join_nul(&[vendor.as_str(), "default"]))))
    }
}

impl SessionUid {
    /// A fresh UUIDv7 uid (World mints on first sight, §3.3.2 rule 5).
    pub fn mint() -> SessionUid {
        SessionUid(format!("s_{}", uuid::Uuid::now_v7()))
    }
}

impl TeamKey {
    /// sha256(source + "\0" + accountId + "\0" + nativeTeamId). `source` is the
    /// `TeamSource` wire name; an unknown account contributes `""`.
    pub fn derive(source: &str, account: Option<&AccountId>, native_team_id: &str) -> TeamKey {
        let account = account.map_or("", AccountId::as_str);
        TeamKey(format!(
            "t_{}",
            hash16(&join_nul(&[source, account, native_team_id]))
        ))
    }
}

impl MemberKey {
    /// sha256(teamKey + "\0" + agentId + "\0" + joinedAt) with `joinedAt` as the
    /// decimal epoch-ms string (`""` when unknown).
    pub fn derive(team: &TeamKey, agent_id: &str, joined_at: Option<EpochMs>) -> MemberKey {
        let joined = joined_at.map(|v| v.to_string()).unwrap_or_default();
        MemberKey(format!(
            "m_{}",
            hash16(&join_nul(&[team.as_str(), agent_id, &joined]))
        ))
    }
}

impl TaskKey {
    /// sha256(teamKey + "\0" + nativeTaskId).
    pub fn derive(team: &TeamKey, native_task_id: &str) -> TaskKey {
        TaskKey(format!("k_{}", hash16(&join_nul(&[team.as_str(), native_task_id]))))
    }

    /// Inferred items: sha256(teamKey + "\0inferred\0" + memberKey + "\0" + openingRowUuid).
    pub fn derive_inferred(team: &TeamKey, member: &MemberKey, opening_row_uuid: &str) -> TaskKey {
        TaskKey(format!(
            "k_{}",
            hash16(&join_nul(&[team.as_str(), "inferred", member.as_str(), opening_row_uuid]))
        ))
    }
}

impl MessageKey {
    /// sha256(teamKey + "\0" + discriminator), where the discriminator is the
    /// `msg_id` or `from+to+at+ordinal` (the caller concatenates).
    pub fn derive(team: &TeamKey, discriminator: &str) -> MessageKey {
        MessageKey(format!("g_{}", hash16(&join_nul(&[team.as_str(), discriminator]))))
    }
}

impl ProcessKey {
    /// `x_<pid>_<kernel start seconds>`.
    pub fn new(pid: u32, start_sec: u64) -> ProcessKey {
        ProcessKey(format!("x_{pid}_{start_sec}"))
    }
}

impl PtyId {
    pub fn mint() -> PtyId {
        PtyId(format!("y_{}", uuid::Uuid::now_v7()))
    }
}

impl PaneId {
    /// `%<n>`.
    pub fn new(n: u32) -> PaneId {
        PaneId(format!("%{n}"))
    }
}

impl EpisodeId {
    /// sha256(uid + "\0" + stateKind + "\0" + enteredAt).
    pub fn derive(uid: &SessionUid, state_kind: &str, entered_at: EpochMs) -> EpisodeId {
        EpisodeId(format!(
            "e_{}",
            hash16(&join_nul(&[uid.as_str(), state_kind, &entered_at.to_string()]))
        ))
    }
}

impl QuitRecordId {
    pub fn mint() -> QuitRecordId {
        QuitRecordId(format!("r_{}", uuid::Uuid::now_v7()))
    }
}

impl ConfirmToken {
    pub fn mint() -> ConfirmToken {
        ConfirmToken(format!("c_{}", random_hex32()))
    }
}

impl UndoToken {
    pub fn mint() -> UndoToken {
        UndoToken(format!("u_{}", random_hex32()))
    }
}

impl SubId {
    pub fn mint() -> SubId {
        SubId(format!("v_{}", &random_hex32()[..16]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_match_their_patterns() {
        assert!(SessionUid::mint().validate().is_ok());
        assert!(PtyId::mint().validate().is_ok());
        assert!(QuitRecordId::mint().validate().is_ok());
        assert!(ConfirmToken::mint().validate().is_ok());
        assert!(UndoToken::mint().validate().is_ok());
        assert!(SubId::mint().validate().is_ok());
        assert!(ProcessKey::new(4242, 1_761_000_000).validate().is_ok());
        assert!(PaneId::new(0).validate().is_ok());
    }

    #[test]
    fn derivations_are_stable_and_shaped() {
        let p = ProjectId::derive("/Users/x/repo/.git");
        assert_eq!(p, ProjectId::derive("/Users/x/repo/.git"));
        assert!(p.validate().is_ok());
        let a = AccountId::derive(Vendor::Claude, "/Users/x/.claude");
        assert_ne!(a, AccountId::derive(Vendor::Codex, "/Users/x/.claude"));
        assert!(AccountId::derive_default_login(Vendor::Agy).validate().is_ok());
        let t = TeamKey::derive("claudeNative", Some(&a), "session-25ae1157");
        let m1 = MemberKey::derive(&t, "db@session-25ae1157", Some(1));
        let m2 = MemberKey::derive(&t, "db@session-25ae1157", Some(2));
        assert_ne!(m1, m2, "a reused name with a new joinedAt is a new member");
        assert!(TaskKey::derive_inferred(&t, &m1, "u-1").validate().is_ok());
        assert!(EpisodeId::derive(&SessionUid::mint(), "needsApproval", 5).validate().is_ok());
        // sha256("a") = ca978112ca1bbdca…
        assert_eq!(hash16(b"a"), "ca978112ca1bbdca");
    }

    #[test]
    fn patterns_reject_near_misses() {
        assert!(SessionUid::parse("s_0192f000-0000-7000-8000-000000000000").is_ok());
        assert!(SessionUid::parse("s_0192f000-0000-4000-8000-000000000000").is_err()); // v4
        assert!(SessionUid::parse("s_0192F000-0000-7000-8000-000000000000").is_err()); // upper
        assert!(SessionUid::parse("0192f000-0000-7000-8000-000000000000").is_err());
        assert!(ClientMsgId::parse("0192f000-0000-4000-8000-000000000000").is_ok());
        assert!(ClientMsgId::parse("0192f000-0000-4000-c000-000000000000").is_err()); // variant
        assert!(ProjectId::parse("p_0123456789abcdef").is_ok());
        assert!(ProjectId::parse("p_0123456789abcde").is_err());
        assert!(ProcessKey::parse("x_1_2").is_ok());
        assert!(ProcessKey::parse("x_12345678901_2").is_err());
        assert!(PaneId::parse("%0").is_ok());
        assert!(PaneId::parse("%1234567").is_err());
        assert!(RequestId::parse("a".repeat(128)).is_ok());
        assert!(RequestId::parse("a".repeat(129)).is_err());
        assert!(RequestId::parse("").is_err());
        assert!(RequestId::parse("tab\there").is_err());
        assert!(ViewId::parse("tile-1").is_ok());
        assert!(ViewId::parse("Tile").is_err());
        assert!(BlockId::parse("msg_01#3").is_ok());
        assert!(HistoryCursor::parse("abc_-=").is_ok());
        assert!(HistoryCursor::parse("a+b").is_err());
    }
}
