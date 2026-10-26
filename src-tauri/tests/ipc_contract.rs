//! The IPC contract suite (Gilde 2 contract §14.2; owner T01).
//!
//! * **T-contract-*** — a command that is not in the manifest is rejected by
//!   the real capability ACL; the exact CSP string; the exact capability set;
//!   no `bigint` in the generated TypeScript; the registered handler list equals
//!   the manifest.
//! * **T-ipc-1 generic cases, for every command of the manifest** — a valid
//!   request (wave 0: `UNSUPPORTED{notBuilt}`), wrong shape, missing `req`,
//!   unknown field, unknown envelope key, oversize (+1 byte over the cap, and
//!   exactly at the cap), and JSON depth.
//!
//! Every call goes through Tauri's own IPC path on the mock runtime with the
//! context `build.rs` generated (`generate_context!(test = true)`), so the ACL
//! that rejects non-manifest commands is the one the app ships.

mod fixtures;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use gilde_code_lib::commands::registry;
use gilde_code_lib::contract::ipc_manifest::{self, CapabilityFile, COMMANDS, CORE_PERMISSIONS};
use serde_json::{json, Value};
use tauri::test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindow;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src ipc: http://ipc.localhost; object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'";

// -- a mock app with the real context ------------------------------------------------

struct Harness {
    _app: tauri::App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    _env: fixtures::Hermetic,
}

fn harness() -> Harness {
    let env = fixtures::Hermetic::new();
    let app = mock_builder()
        .invoke_handler(registry::handler())
        .build(tauri::generate_context!(test = true))
        .expect("build the mock app with the generated context");
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("the main window");
    Harness {
        _app: app,
        webview,
        _env: env,
    }
}

impl Harness {
    fn invoke(&self, cmd: &str, args: Value) -> Result<Value, Value> {
        get_ipc_response(
            &self.webview,
            InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::Json(args),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().unwrap_or(Value::Null))
    }
}

/// `Err(IpcError)` with this code and these params.
fn assert_ipc_error(result: &Result<Value, Value>, code: &str, params: &[(&str, &str)], what: &str) {
    let err = match result {
        Err(e) => e,
        Ok(v) => panic!("{what}: expected {code}, got Ok({v})"),
    };
    assert_eq!(err["code"], code, "{what}: {err}");
    for (k, v) in params {
        assert_eq!(err["params"][k], *v, "{what}: params.{k} in {err}");
    }
    let obj = err.as_object().unwrap_or_else(|| panic!("{what}: not an IpcError object: {err}"));
    let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        BTreeSet::from(["code", "key", "params", "detail", "retryable"]),
        "{what}: the one error shape (§4.4)"
    );
    assert!(err["key"].as_str().is_some_and(|k| !k.is_empty()), "{what}: key");
}

// -- one valid request per command --------------------------------------------------

const UID: &str = "s_0192f000-0000-7000-8000-000000000001";
const CRID: &str = "0192f000-0000-4000-8000-000000000001";
const PROJECT: &str = "p_0123456789abcdef";
const ACCOUNT: &str = "a_0123456789abcdef";
const TEAM: &str = "t_0123456789abcdef";
const MEMBER: &str = "m_0123456789abcdef";
const TASK: &str = "k_0123456789abcdef";
const MESSAGE: &str = "g_0123456789abcdef";
const PROCESS: &str = "x_123_456";
const PTY: &str = "y_0192f000-0000-7000-8000-000000000001";
const RECORD: &str = "r_0192f000-0000-7000-8000-000000000001";
const CONFIRM: &str = "c_0123456789abcdef0123456789abcdef";
const UNDO: &str = "u_0123456789abcdef0123456789abcdef";
const SUB: &str = "v_0123456789abcdef";
const VIEW: &str = "main-1";

/// A request every value rule accepts (§4.2). A manifest command without a
/// sample fails the suite: T-ipc-1 must cover every command.
fn sample(name: &str) -> Value {
    match name {
        "app_bootstrap" | "app_input_activity" | "store_subscribe" | "app_quit_preview"
        | "app_quit_cancel" | "agents_rescan" | "app_diagnostics" | "project_pick" | "ide_editors"
        | "health_processes" | "notify_resume" | "notify_permission_request" | "notify_test"
        | "settings_get" => json!({}),
        "app_mark" => json!({"mark": "firstPaint", "perfMs": 120}),
        "app_view_state" => json!({"screen": "home", "visibleUids": [UID], "focusedUid": null, "zoomedUid": null, "teamKey": null, "documentVisible": true}),
        "ui_state_set" => json!({"key": "lastView", "value": {"screen": "home"}}),
        "app_quit" => json!({"confirmToken": CONFIRM}),
        "continue_all" => json!({"recordId": RECORD, "uids": null}),
        "quit_record_dismiss" => json!({"recordId": RECORD}),
        "newsession_options" => json!({"projectId": PROJECT, "vendor": "claude"}),
        "session_start" => json!({
            "clientRequestId": CRID, "projectId": PROJECT, "vendor": "claude", "accountId": ACCOUNT,
            "model": null, "location": {"kind": "newWorktree", "slug": "fix-login", "base": null},
            "permissionMode": "default", "controlMode": "conversation", "team": false, "prompt": "hello"
        }),
        "session_continue" => json!({"clientRequestId": CRID, "uid": UID, "how": {"kind": "resume"}, "controlMode": null, "model": null, "permissionMode": null}),
        "session_retry_as_terminal" => json!({"clientRequestId": CRID, "uid": UID}),
        "session_stop" => json!({"uid": UID, "confirmToken": CONFIRM}),
        "session_dismiss" => json!({"uid": UID, "what": "newReply"}),
        "session_stop_preview" | "session_mark_seen" | "session_models" | "session_technical"
        | "turn_force_stop" | "changes_get" | "git_merge_preview" | "worktree_discard_preview"
        | "owner_focus" | "history_preview" | "history_row" | "queue_later" => json!({"uid": UID}),
        "turn_send" => json!({"uid": UID, "clientMsgId": CRID, "text": "hello", "priority": "next"}),
        "turn_cancel_queued" | "turn_retry" => json!({"uid": UID, "clientMsgId": CRID}),
        "turn_interrupt" => json!({"uid": UID, "discardQueued": false}),
        "session_set_model" => json!({"uid": UID, "model": "claude-opus-5-5"}),
        "session_set_permission_mode" => json!({"uid": UID, "mode": "default"}),
        "interaction_answer" => json!({"uid": UID, "requestId": "req-1", "decision": {"kind": "allowOnce"}, "via": "button"}),
        "screen_prompt_key" => json!({"uid": UID, "requestId": "sp_0123456789abcdef", "promptRev": "0123456789abcdef", "action": {"kind": "key", "key": "1"}}),
        "interaction_detail" => json!({"uid": UID, "requestId": "req-1"}),
        "convo_subscribe" => json!({"uid": UID, "viewId": VIEW, "tailBlocks": null}),
        "convo_unsubscribe" => json!({"subId": SUB}),
        "convo_load_older" => json!({"subId": SUB, "beforeBlockId": "msg_1#0", "count": 50}),
        "content_image" => json!({"imageId": "i_00000000000000a1"}),
        "pty_attach" | "pty_detach" => json!({"ptyId": PTY, "viewId": VIEW}),
        "pty_snapshot" => json!({"ptyId": PTY}),
        "pty_write" => json!({"ptyId": PTY, "b64": "aGk="}),
        "pty_paste" => json!({"ptyId": PTY, "text": "ls", "submit": true}),
        "pty_resize" => json!({"ptyId": PTY, "cols": 120, "rows": 40}),
        "git_commit_preview" => json!({"uid": UID, "paths": ["src/a.rs"]}),
        "git_commit" => json!({"uid": UID, "message": "fix", "paths": ["src/a.rs"], "confirmToken": CONFIRM}),
        "git_merge" => json!({"uid": UID, "confirmToken": CONFIRM}),
        "worktree_discard" => json!({"uid": UID, "confirmToken": CONFIRM, "acceptedLosses": ["uncommitted"]}),
        "ide_open" => json!({"target": {"kind": "file", "uid": UID, "path": "src/a.rs", "line": 3}}),
        "ide_reveal" => json!({"target": {"kind": "transcript", "uid": UID}}),
        "link_open" => json!({"url": "https://example.com/docs"}),
        "team_send" => json!({"teamKey": TEAM, "memberKey": MEMBER, "clientMsgId": CRID, "text": "hi"}),
        "team_member_close_preview" => json!({"teamKey": TEAM, "memberKey": MEMBER}),
        "team_member_close" => json!({"teamKey": TEAM, "memberKey": MEMBER, "confirmToken": CONFIRM}),
        "team_message_body" => json!({"messageKey": MESSAGE}),
        "team_messages_page" => json!({"teamKey": TEAM, "before": 1_761_000_000_000_i64, "count": 50}),
        "team_task_title" => json!({"taskKey": TASK}),
        "account_add" => json!({"vendor": "claude"}),
        "account_confirm" | "account_remove" | "account_set_default" | "quota_refresh" => json!({"accountId": ACCOUNT}),
        "account_set_label" => json!({"accountId": ACCOUNT, "label": "Work"}),
        "account_sign_in" => json!({"clientRequestId": CRID, "accountId": ACCOUNT}),
        "health_process_detail" => json!({"processKey": PROCESS}),
        "health_stop_preview" => json!({"processKeys": [PROCESS]}),
        "health_stop" => json!({"confirmToken": CONFIRM}),
        "undo_cancel" => json!({"undoToken": UNDO}),
        "history_query" => json!({"queryId": "q-1", "text": "", "filters": {"projectId": null, "vendor": null, "from": null, "to": null, "accountId": null, "origin": null}, "cursor": null, "limit": 20}),
        "notify_pause" => json!({"until": "oneHour"}),
        "settings_set" => json!({"patch": {"locale": "ko", "theme": null, "attention": null, "notify": null, "ide": null}}),
        "settings_set_executable" => json!({"key": "agents.envPassthrough", "value": {"kind": "envNames", "names": ["HTTPS_PROXY"]}}),
        other => panic!("T-ipc-1: no sample request for `{other}` — every manifest command needs one"),
    }
}

/// The invoke arguments for `req` (plus a channel for the streaming commands).
fn args(spec: &ipc_manifest::CommandSpec, req: Value) -> Value {
    let mut a = json!({ "req": req });
    if spec.channel.is_some() {
        a["channel"] = json!("__CHANNEL__:1");
    }
    a
}

// -- T-contract-* -------------------------------------------------------------------

#[test]
fn t_contract_non_manifest_commands_are_rejected() {
    let h = harness();
    // Deleted from the IPC surface (§4.8, SP-6), plus a name nobody declared.
    for cmd in ["session_open", "dispatch_create", "graph_snapshot", "cco_team_list", "mesh_quota", "app_reveal", "app_settings_set", "procmon_kill", "no_such_command"] {
        let r = h.invoke(cmd, json!({"req": {}}));
        let err = r.expect_err(&format!("{cmd} must not be invocable"));
        assert!(
            err.get("code").is_none(),
            "{cmd}: rejected by the ACL before any command ran, not by a command: {err}"
        );
        let text = err.as_str().unwrap_or_default();
        assert!(text.contains(cmd), "{cmd}: the rejection names the command: {text}");
    }
}

#[test]
fn t_contract_every_manifest_command_is_registered_and_reaches_its_stub() {
    assert_eq!(
        registry::REGISTERED,
        COMMANDS.iter().map(|c| c.name).collect::<Vec<_>>().as_slice(),
        "the generated generate_handler! list equals the manifest (no extra, no missing)"
    );
    let h = harness();
    for spec in COMMANDS {
        let r = h.invoke(spec.name, args(spec, sample(spec.name)));
        assert_ipc_error(&r, "UNSUPPORTED", &[("cap", "notBuilt")], spec.name);
        assert_eq!(r.unwrap_err()["retryable"], false);
    }
}

#[test]
fn t_contract_csp_and_window_hardening() {
    let conf: Value = read_json(&manifest_dir().join("tauri.conf.json"));
    assert_eq!(conf["app"]["security"]["csp"], CSP, "the exact CSP string of §9.2");
    assert_eq!(conf["app"]["security"]["freezePrototype"], true);
    assert_eq!(conf["app"]["withGlobalTauri"], false);
    assert!(conf["app"]["security"].get("assetProtocol").is_none(), "no assetProtocol");
    for w in conf["app"]["windows"].as_array().unwrap() {
        assert!(w.get("url").is_none_or(|u| !u.as_str().unwrap_or("").contains("://")), "no remote URL in any window");
    }
}

#[test]
fn t_contract_exact_capability_set() {
    let dir = manifest_dir().join("capabilities");
    let files: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, BTreeSet::from(["debug.json".to_string(), "main.json".to_string()]), "default.json is gone");

    let main: Value = read_json(&dir.join("main.json"));
    let debug: Value = read_json(&dir.join("debug.json"));
    let perms = |v: &Value| -> BTreeSet<String> {
        v["permissions"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect()
    };
    let mut expected_main: BTreeSet<String> = CORE_PERMISSIONS.iter().map(|s| s.to_string()).collect();
    let mut expected_debug = BTreeSet::new();
    for c in COMMANDS {
        let p = ipc_manifest::permission_id(c.name);
        match c.capability {
            CapabilityFile::Main => expected_main.insert(p),
            CapabilityFile::Debug => expected_debug.insert(p),
        };
    }
    assert_eq!(perms(&main), expected_main, "main.json grants exactly the core set + the main manifest");
    assert_eq!(perms(&debug), expected_debug, "debug.json grants exactly the debug commands");
    for (v, file) in [(&main, "main"), (&debug, "debug")] {
        assert_eq!(v["windows"], json!(["main"]), "{file}");
        assert!(v.get("remote").is_none(), "{file}: no remote origin");
        for p in perms(v) {
            for banned in ["core:default", "core:event:allow-emit", "opener:", "dialog:", "notification:", "fs:", "shell:", "core:webview:"] {
                assert!(!p.starts_with(banned), "{file}: {p} is not granted (§9.2)");
            }
        }
    }
}

/// Debug builds include `capabilities/debug.json` (build.rs); release builds do not.
#[cfg(debug_assertions)]
#[test]
fn t_contract_debug_commands_answer_only_in_debug_builds() {
    let h = harness();
    let spec = ipc_manifest::command("app_diagnostics").unwrap();
    assert_eq!(spec.capability, CapabilityFile::Debug);
    let r = h.invoke("app_diagnostics", args(spec, json!({})));
    assert_ipc_error(&r, "UNSUPPORTED", &[("cap", "notBuilt")], "app_diagnostics");
}

#[test]
fn t_contract_ints_no_bigint_in_generated_typescript() {
    let root = manifest_dir().parent().unwrap().to_path_buf();
    let mut scanned = 0;
    for dir in [root.join("src/lib/gen"), root.join("src-tauri/sidecar/src/gen")] {
        for file in walk(&dir) {
            if file.extension().is_some_and(|e| e == "ts") {
                let text = std::fs::read_to_string(&file).unwrap();
                assert!(!text.contains("bigint"), "{} contains bigint (§3.1 rule 3)", file.display());
                scanned += 1;
            }
        }
    }
    assert!(scanned > 100, "the generated TypeScript exists (run scripts/gen-ipc.mjs): {scanned} files");
}

#[test]
fn t_contract_permissions_exist_for_every_command() {
    let dir = manifest_dir().join("permissions/autogenerated");
    for c in COMMANDS {
        let text = std::fs::read_to_string(dir.join(format!("{}.toml", c.name)))
            .unwrap_or_else(|e| panic!("{}: autogenerated permission missing: {e}", c.name));
        assert!(text.contains(&format!("identifier = \"{}\"", ipc_manifest::permission_id(c.name))));
    }
}

// -- T-ipc-1 generic cases, every command ------------------------------------------------

#[test]
fn t_ipc_1_wrong_shape_is_invalid_argument_shape() {
    let h = harness();
    for spec in COMMANDS {
        for bad in [json!(42), json!("x"), json!([]), json!(null)] {
            let r = h.invoke(spec.name, args(spec, bad.clone()));
            assert_ipc_error(&r, "INVALID_ARGUMENT", &[("field", "$"), ("rule", "shape")], &format!("{} req={bad}", spec.name));
        }
        // `req` missing altogether.
        let mut a = args(spec, json!({}));
        a.as_object_mut().unwrap().remove("req");
        let r = h.invoke(spec.name, a);
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("rule", "shape")], &format!("{} without req", spec.name));
    }
}

#[test]
fn t_ipc_1_unknown_field_is_rejected() {
    let h = harness();
    for spec in COMMANDS {
        let mut req = sample(spec.name);
        req.as_object_mut().unwrap().insert("zzUnknown".into(), json!(true));
        let r = h.invoke(spec.name, args(spec, req));
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("field", "$"), ("rule", "shape")], &format!("{} unknown field", spec.name));
    }
}

#[test]
fn t_ipc_1_unknown_field_in_a_nested_unit_variant_is_rejected() {
    // serde ignores unknown fields on unit variants of internally tagged enums;
    // the contract pipeline must not.
    let h = harness();
    let spec = ipc_manifest::command("interaction_answer").unwrap();
    let mut req = sample("interaction_answer");
    req["decision"] = json!({"kind": "allowOnce", "message": "smuggled"});
    let r = h.invoke(spec.name, args(spec, req));
    assert_ipc_error(&r, "INVALID_ARGUMENT", &[("rule", "shape")], "decision unit variant with a field");
}

#[test]
fn t_ipc_1_positional_arrays_are_rejected() {
    // serde would accept a struct as a positional array; the contract does not.
    let h = harness();
    let spec = ipc_manifest::command("history_query").unwrap();
    let mut req = sample("history_query");
    req["filters"] = json!([null, null, null, null, null, null]);
    let r = h.invoke(spec.name, args(spec, req));
    assert_ipc_error(&r, "INVALID_ARGUMENT", &[("rule", "shape")], "positional filters");
    let spec = ipc_manifest::command("session_mark_seen").unwrap();
    let r = h.invoke(spec.name, args(spec, json!([UID])));
    assert_ipc_error(&r, "INVALID_ARGUMENT", &[("rule", "shape")], "positional request");
}

#[test]
fn t_ipc_1_unknown_envelope_key_is_rejected() {
    let h = harness();
    for spec in COMMANDS {
        let mut a = args(spec, sample(spec.name));
        a.as_object_mut().unwrap().insert("extra".into(), json!(1));
        let r = h.invoke(spec.name, a);
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("field", "$"), ("rule", "envelope")], spec.name);
    }
}

#[test]
fn t_ipc_1_size_cap_plus_one_byte_and_at_the_cap() {
    let h = harness();
    for spec in COMMANDS {
        // {"pad":"…"} is 10 bytes of JSON around the padding.
        let over = json!({ "pad": "x".repeat(spec.max_body_bytes + 1 - 10) });
        assert_eq!(serde_json::to_string(&over).unwrap().len(), spec.max_body_bytes + 1);
        let r = h.invoke(spec.name, args(spec, over));
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("field", "$"), ("rule", "size")], &format!("{} +1 byte", spec.name));

        let at = json!({ "pad": "x".repeat(spec.max_body_bytes - 10) });
        assert_eq!(serde_json::to_string(&at).unwrap().len(), spec.max_body_bytes);
        let r = h.invoke(spec.name, args(spec, at));
        // At the cap the size guard passes; the unknown field then fails the shape.
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("rule", "shape")], &format!("{} at the cap", spec.name));
    }
}

#[test]
fn t_ipc_1_json_depth_over_64_is_rejected() {
    let h = harness();
    let mut deep = json!(0);
    for _ in 0..65 {
        deep = json!([deep]);
    }
    for spec in COMMANDS {
        let r = h.invoke(spec.name, args(spec, json!({ "nest": deep.clone() })));
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("field", "$"), ("rule", "depth")], spec.name);
    }
}

#[test]
fn t_ipc_1_value_rules_report_field_and_rule() {
    let h = harness();
    let cases: Vec<(&str, Value, &str, &str)> = vec![
        ("app_mark", json!({"mark": "firstPaint", "perfMs": 600_001}), "perfMs", "range"),
        ("session_mark_seen", json!({"uid": "s_not-a-uuid"}), "uid", "pattern"),
        ("turn_send", json!({"uid": UID, "clientMsgId": CRID, "text": "", "priority": "next"}), "text", "required"),
        ("turn_send", json!({"uid": UID, "clientMsgId": CRID, "text": "a\u{0}b", "priority": "next"}), "text", "nul"),
        ("turn_send", json!({"uid": UID, "clientMsgId": CRID, "text": "x", "priority": "next", "images": [{"mediaType": "image/svg+xml", "data": "PHN2Zz4="}]}), "images[0].mediaType", "pattern"),
        ("content_image", json!({"imageId": "../etc"}), "imageId", "pattern"),
        ("session_start", {
            let mut r = sample("session_start");
            r["location"] = json!({"kind": "newWorktree", "slug": "Bad Slug", "base": null});
            r
        }, "location.slug", "pattern"),
        ("convo_subscribe", json!({"uid": UID, "viewId": VIEW, "tailBlocks": 0}), "tailBlocks", "range"),
        ("link_open", json!({"url": "javascript:alert(1)"}), "url", "url"),
        ("link_open", json!({"url": "https://user:pw@example.com/"}), "url", "url"),
        ("settings_set_executable", json!({"key": "agents.envPassthrough", "value": {"kind": "envNames", "names": ["TMUX"]}}), "value.names[0]", "reserved"),
        ("settings_set", json!({"patch": {"locale": null, "theme": null, "attention": {"noProgressAfterSecs": 59, "watchedFinishedTurns": null}, "notify": null, "ide": null}}), "patch.attention.noProgressAfterSecs", "range"),
        ("health_stop_preview", json!({"processKeys": []}), "processKeys", "minItems"),
        ("pty_write", json!({"ptyId": PTY, "b64": "not base64!"}), "b64", "pattern"),
        ("interaction_answer", json!({"uid": UID, "requestId": "req-1", "decision": {"kind": "denyWithMessage", "message": ""}, "via": "button"}), "decision.message", "required"),
        ("ui_state_set", json!({"key": "teamLayout:t_bad", "value": 1}), "key", "pattern"),
        ("ide_open", json!({"target": {"kind": "file", "uid": UID, "path": "a.rs", "line": 0}}), "target.line", "range"),
        ("history_query", json!({"queryId": "q-1", "text": "", "filters": {"projectId": null, "vendor": null, "from": 20, "to": 10, "accountId": null, "origin": null}, "cursor": null, "limit": 20}), "filters.from", "range"),
    ];
    for (cmd, req, field, rule) in cases {
        let spec = ipc_manifest::command(cmd).unwrap();
        let r = h.invoke(cmd, args(spec, req));
        assert_ipc_error(&r, "INVALID_ARGUMENT", &[("field", field), ("rule", rule)], &format!("{cmd} {field}"));
    }
}

#[test]
fn t_ipc_1_absent_and_empty_optionals_take_the_default() {
    let h = harness();
    // session_start: `model` absent, `""` or null all mean "vendor default";
    // `prompt` "" means "no first message" (§3.1 rule 4).
    let spec = ipc_manifest::command("session_start").unwrap();
    for (model, prompt) in [(json!(null), json!(null)), (json!(""), json!(""))] {
        let mut r = sample("session_start");
        r["model"] = model;
        r["prompt"] = prompt;
        let res = h.invoke(spec.name, args(spec, r));
        assert_ipc_error(&res, "UNSUPPORTED", &[("cap", "notBuilt")], "session_start defaults");
    }
    let mut absent = sample("session_start");
    absent.as_object_mut().unwrap().remove("model");
    absent.as_object_mut().unwrap().remove("prompt");
    let res = h.invoke(spec.name, args(spec, absent));
    assert_ipc_error(&res, "UNSUPPORTED", &[("cap", "notBuilt")], "session_start absent optionals");
}

// -- helpers -------------------------------------------------------------------------

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn every_manifest_command_has_a_sample_and_unique_permission() {
    let mut seen = HashMap::new();
    for c in COMMANDS {
        let _ = sample(c.name);
        assert!(seen.insert(ipc_manifest::permission_id(c.name), c.name).is_none());
    }
}
