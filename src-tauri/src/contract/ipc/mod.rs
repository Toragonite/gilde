//! Request and response types of every command (contract §4.2).
//!
//! Requests carry `deny_unknown_fields` and implement [`Validate`] with the
//! value rules stated in §4.2 (plus the default caps of §4.1 rule 7: strings
//! ≤ 64 KiB, lists ≤ 1,000 items, unless stated). Responses are plain
//! `rename_all = "camelCase"` structs named `<Name>Res`.
//!
//! [`Validate`]: crate::contract::validate::Validate

pub mod accounts;
pub mod app;
pub mod changes;
pub mod content;
pub mod history;
pub mod sessions;
pub mod teams;
pub mod turns;

pub use accounts::*;
pub use app::*;
pub use changes::*;
pub use content::*;
pub use history::*;
pub use sessions::*;
pub use teams::*;
pub use turns::*;

/// Declares empty request/response structs (`{}` on the wire). Requests reject
/// any field (`deny_unknown_fields`).
macro_rules! empty_structs {
    (req: $($(#[$m:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$m])*
            #[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, ts_rs::TS)]
            #[serde(deny_unknown_fields)]
            #[cfg_attr(feature = "ts-export", ts(export))]
            pub struct $name {}

            impl crate::contract::validate::Validate for $name {}
        )*
    };
    (res: $($(#[$m:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$m])*
            #[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, ts_rs::TS)]
            #[cfg_attr(feature = "ts-export", ts(export))]
            pub struct $name {}
        )*
    };
}
pub(crate) use empty_structs;

/// The `model` rule of §4.2.2: `^[A-Za-z0-9._:\[\]/-]{1,128}$`.
pub fn is_model_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'[' | b']' | b'/' | b'-')
        })
}

/// Value rule for an optional model field: absent or `""` = vendor default.
pub fn model_rule(field: &str, model: &Option<String>) -> crate::contract::validate::Checked {
    crate::contract::validate::optional_str(field, model, |f, s| {
        if is_model_id(s) {
            Ok(())
        } else {
            Err(crate::contract::validate::Invalid::new(f, "pattern"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids() {
        assert!(is_model_id("claude-opus-5-5"));
        assert!(is_model_id("us.anthropic.claude[1m]"));
        assert!(is_model_id("gpt-5.1-codex/high:fast"));
        assert!(!is_model_id(""));
        assert!(!is_model_id("bad model"));
        assert!(!is_model_id(&"a".repeat(129)));
    }

    #[test]
    fn turn_send_body_cap_holds_its_images() {
        assert_eq!(
            crate::contract::ipc_manifest::BODY_TURN_SEND,
            300 * 1024 + MAX_SEND_IMAGES * (MAX_SEND_IMAGE_BASE64 + 1024),
            "the manifest spells the image caps out"
        );
    }

    #[test]
    fn turn_send_takes_raster_base64_images_and_an_image_needs_no_text() {
        use crate::contract::validate::Validate as _;
        let req = |text: &str, images: Vec<OutgoingImage>| TurnSendReq {
            uid: crate::contract::ids::SessionUid::parse("s_0192f000-0000-7000-8000-000000000001").unwrap(),
            client_msg_id: crate::contract::ids::ClientMsgId::parse("0192f000-0000-4000-8000-000000000001").unwrap(),
            text: text.into(),
            priority: SendPriority::Next,
            images,
        };
        let png = || OutgoingImage { media_type: "image/png".into(), data: "iVBORw0KGgo=".into() };
        assert!(req("", vec![]).validate().is_err(), "no text and no image");
        assert!(req("", vec![png()]).validate().is_ok(), "an image alone");
        assert!(req("look", vec![png(); MAX_SEND_IMAGES]).validate().is_ok());
        let rule = |r: TurnSendReq| r.validate().unwrap_err().rule;
        assert_eq!(rule(req("look", vec![png(); MAX_SEND_IMAGES + 1])), "maxItems");
        assert_eq!(rule(req("x", vec![OutgoingImage { media_type: "image/svg+xml".into(), data: "PHN2Zz4=".into() }])), "pattern");
        assert_eq!(rule(req("x", vec![OutgoingImage { media_type: "image/png".into(), data: "not base64".into() }])), "pattern");
        assert_eq!(
            rule(req("x", vec![OutgoingImage { media_type: "image/png".into(), data: "A".repeat(MAX_SEND_IMAGE_BASE64 + 4) }])),
            "maxLength"
        );
    }
}
