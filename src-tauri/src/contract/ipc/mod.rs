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
}
