//! Manifest export for the generator, and manifest self-consistency tests.
//!
//! `export_bindings_ipc_manifest` runs with the ts-rs export tests
//! (`cargo test --features ts-export export_bindings`) and writes the manifest
//! as JSON to the path in `GILDE_IPC_MANIFEST_OUT` (set by `scripts/gen-ipc.mjs`).

use serde_json::{json, Value};

use crate::contract::error::ErrorCode;
use crate::contract::ipc_manifest::{self, CapabilityFile, COMMANDS, EVENTS};

/// The manifest as the generator reads it.
pub fn manifest_json() -> Value {
    let commands: Vec<Value> = COMMANDS
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "section": c.section,
                "module": c.module,
                "owner": c.owner,
                "req": c.req,
                "res": c.res,
                "channel": c.channel,
                "errors": c.errors.iter().map(|e| json!({"code": e.code, "params": e.params})).collect::<Vec<_>>(),
                "idempotency": c.idempotency.as_str(),
                "rate": c.rate.as_str(),
                "capability": c.capability.as_str(),
                "maxBodyBytes": c.max_body_bytes,
                "permission": ipc_manifest::permission_id(c.name),
            })
        })
        .collect();
    let events: Vec<Value> = EVENTS
        .iter()
        .map(|e| json!({"name": e.name, "payload": e.payload, "ident": e.ident, "when": e.when}))
        .collect();
    json!({
        "commonErrors": ipc_manifest::COMMON_ERRORS,
        "corePermissions": ipc_manifest::CORE_PERMISSIONS,
        "commands": commands,
        "events": events,
    })
}

#[cfg(feature = "ts-export")]
#[test]
fn export_bindings_ipc_manifest() {
    if let Ok(path) = std::env::var("GILDE_IPC_MANIFEST_OUT") {
        let text = serde_json::to_string_pretty(&manifest_json()).unwrap();
        std::fs::write(path, text + "\n").expect("write the manifest json");
    }
}

#[test]
fn manifest_json_lists_every_command_and_event() {
    let v = manifest_json();
    assert_eq!(v["commands"].as_array().map(Vec::len), Some(COMMANDS.len()));
    assert_eq!(v["events"].as_array().map(Vec::len), Some(4));
    assert_eq!(v["commands"][0]["permission"], "allow-app-bootstrap");
}

#[test]
fn manifest_names_are_unique_snake_case() {
    let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
    for n in &names {
        assert!(
            n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') && !n.starts_with('_'),
            "{n}"
        );
    }
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count, "duplicate command names");
    assert_eq!(count, 84, "the §4.2 list has 84 commands (83 + history_row, A29.2)");
}

#[test]
fn manifest_error_codes_exist() {
    for c in COMMANDS {
        for e in c.errors {
            assert!(ErrorCode::parse(e.code).is_some(), "{}: {}", c.name, e.code);
        }
    }
    for code in ipc_manifest::COMMON_ERRORS {
        assert!(ErrorCode::parse(code).is_some(), "{code}");
    }
}

#[test]
fn only_app_diagnostics_is_debug_only() {
    let debug: Vec<&str> = COMMANDS
        .iter()
        .filter(|c| c.capability == CapabilityFile::Debug)
        .map(|c| c.name)
        .collect();
    assert_eq!(debug, ["app_diagnostics"]);
}

#[test]
fn channels_and_body_caps_follow_the_contract() {
    let with_channel: Vec<(&str, &str)> = COMMANDS
        .iter()
        .filter_map(|c| c.channel.map(|ch| (c.name, ch)))
        .collect();
    assert_eq!(
        with_channel,
        [
            ("store_subscribe", "StorePatch"),
            ("convo_subscribe", "ConvoEnvelope"),
            ("pty_attach", "PtyChunk")
        ]
    );
    for c in COMMANDS {
        let expected = match c.name {
            "pty_write" => ipc_manifest::BODY_PTY_WRITE,
            "turn_send" => ipc_manifest::BODY_TURN_SEND,
            _ => ipc_manifest::BODY_DEFAULT,
        };
        assert_eq!(c.max_body_bytes, expected, "{}", c.name);
    }
}

#[test]
fn permission_ids_are_slugified() {
    assert_eq!(ipc_manifest::permission_id("session_start"), "allow-session-start");
    assert_eq!(ipc_manifest::permission_id("app_bootstrap"), "allow-app-bootstrap");
}
