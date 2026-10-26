//! §4.2.4 Content channels.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::empty_structs;
use crate::contract::common::Notice;
use crate::contract::content::ConvoSourceKind;
use crate::contract::ids::{BlockId, ImageId, PtyId, SessionUid, SubId, ViewId};
use crate::contract::validate::{self as v, Checked, Invalid, Validate};

empty_structs!(res:
    ConvoUnsubscribeRes,
    ConvoLoadOlderRes,
    PtyWriteRes,
    PtyPasteRes,
    PtyResizeRes,
    PtyDetachRes,
);

// -- convo_subscribe ---------------------------------------------------------

/// `tailBlocks` 1..=500 (absent → 200). At most 8 live subscriptions; the 9th
/// evicts the least recently subscribed. First event is always `Reset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ConvoSubscribeReq {
    pub uid: SessionUid,
    pub view_id: ViewId,
    pub tail_blocks: Option<u16>,
}

/// The default of `tailBlocks`.
pub const DEFAULT_TAIL_BLOCKS: u16 = 200;

impl Validate for ConvoSubscribeReq {
    fn validate(&self) -> Checked {
        v::nested("uid", &self.uid)?;
        v::nested("viewId", &self.view_id)?;
        if let Some(n) = self.tail_blocks {
            v::range("tailBlocks", n, 1, 500)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ConvoSubscribeRes {
    pub sub_id: SubId,
    pub source: ConvoSourceKind,
    pub note: Option<Notice>,
}

/// Idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ConvoUnsubscribeReq {
    pub sub_id: SubId,
}

impl Validate for ConvoUnsubscribeReq {
    fn validate(&self) -> Checked {
        v::nested("subId", &self.sub_id)
    }
}

/// `count` 1..=200; ≤ 8 MiB scanned per call; blocks arrive as an `Older` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ConvoLoadOlderReq {
    pub sub_id: SubId,
    pub before_block_id: BlockId,
    pub count: u16,
}

impl Validate for ConvoLoadOlderReq {
    fn validate(&self) -> Checked {
        v::nested("subId", &self.sub_id)?;
        v::nested("beforeBlockId", &self.before_block_id)?;
        v::range("count", self.count, 1, 200)
    }
}

// -- content_image -------------------------------------------------------------

/// The bytes of an image a block names (`ImageRef`), for a `data:` URL. The
/// store keeps images in memory only and drops the oldest first:
/// `NOT_FOUND{image}` once it left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ContentImageReq {
    pub image_id: ImageId,
}

impl Validate for ContentImageReq {
    fn validate(&self) -> Checked {
        v::nested("imageId", &self.image_id)
    }
}

/// `data` is standard base64 of one of `IMAGE_MEDIA_TYPES`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct ContentImageRes {
    pub media_type: String,
    pub data: String,
}

// -- pty_* -------------------------------------------------------------------

/// `{ ptyId, viewId }` (+ `Channel<PtyChunk>` for `pty_attach`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtyAttachReq {
    pub pty_id: PtyId,
    pub view_id: ViewId,
}

impl Validate for PtyAttachReq {
    fn validate(&self) -> Checked {
        v::nested("ptyId", &self.pty_id)?;
        v::nested("viewId", &self.view_id)
    }
}

/// One attached view per PTY: attaching from another view sends
/// `Detached{otherView}` to the previous channel (§6.2.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtyInfo {
    #[ts(type = "number")]
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
    pub title: Option<String>,
}

/// Raw bytes in the response (`GLDP` header + ring, existing format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtySnapshotReq {
    pub pty_id: PtyId,
}

impl Validate for PtySnapshotReq {
    fn validate(&self) -> Checked {
        v::nested("ptyId", &self.pty_id)
    }
}

/// ≤ 1 MiB decoded; only PTYs Gilde owns (all are).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtyWriteReq {
    pub pty_id: PtyId,
    pub b64: String,
}

/// The decoded cap of `pty_write`.
pub const MAX_PTY_WRITE_BYTES: usize = 1024 * 1024;

/// Decoded length of a standard (padded) base64 string, or `None` when it is
/// not well-formed base64.
pub fn base64_decoded_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let pad = b.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return None;
    }
    let body = &b[..b.len() - pad];
    if !body.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/') {
        return None;
    }
    Some(b.len() / 4 * 3 - pad)
}

impl Validate for PtyWriteReq {
    fn validate(&self) -> Checked {
        v::nested("ptyId", &self.pty_id)?;
        if self.b64.is_empty() {
            return Err(Invalid::new("b64", "required"));
        }
        match base64_decoded_len(&self.b64) {
            None => Err(Invalid::new("b64", "pattern")),
            Some(n) if n > MAX_PTY_WRITE_BYTES => Err(Invalid::new("b64", "maxLength")),
            Some(_) => Ok(()),
        }
    }
}

/// Bracketed paste, then CR iff `submit`; ungated by status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtyPasteReq {
    pub pty_id: PtyId,
    pub text: String,
    pub submit: bool,
}

impl Validate for PtyPasteReq {
    fn validate(&self) -> Checked {
        v::nested("ptyId", &self.pty_id)?;
        v::required_str("text", &self.text, v::DEFAULT_MAX_STRING_BYTES)
    }
}

/// Values are clamped to cols 20..=1000, rows 5..=500 and no-op sizes are
/// ignored (existing behaviour) — never rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtyResizeReq {
    pub pty_id: PtyId,
    pub cols: u16,
    pub rows: u16,
}

impl Validate for PtyResizeReq {
    fn validate(&self) -> Checked {
        v::nested("ptyId", &self.pty_id)
    }
}

/// Idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct PtyDetachReq {
    pub pty_id: PtyId,
    pub view_id: ViewId,
}

impl Validate for PtyDetachReq {
    fn validate(&self) -> Checked {
        v::nested("ptyId", &self.pty_id)?;
        v::nested("viewId", &self.view_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_lengths() {
        assert_eq!(base64_decoded_len(""), Some(0));
        assert_eq!(base64_decoded_len("YQ=="), Some(1));
        assert_eq!(base64_decoded_len("YWI="), Some(2));
        assert_eq!(base64_decoded_len("YWJj"), Some(3));
        assert_eq!(base64_decoded_len("YWJ"), None);
        assert_eq!(base64_decoded_len("Y$=="), None);
        assert_eq!(base64_decoded_len("Y==="), None);
    }
}
