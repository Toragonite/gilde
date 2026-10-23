//! `fake_real_tmux` — records its argv and env to a file and exits with a
//! configured code (contract §14.1, T-shim-passthrough).
//!
//! Owner: T07. Behind the `test-fixtures` feature; never shipped. Shell-free:
//! it runs nothing.
//!
//! * `FAKE_REAL_TMUX_RECORD=<absolute path>`: append one JSON line
//!   `{"argv0": <hex>, "args": [<hex>…], "env": {<key>: <value>…}}` (argv as
//!   hex of the raw bytes, so passthrough is checked byte-for-byte; env keys
//!   and values lossily as UTF-8).
//! * `FAKE_REAL_TMUX_EXIT=<n>`: the exit code (default 0).
//! * `-V` prints `tmux 9.9-fake` (a version the shim never prints itself).

use std::io::Write;
use std::os::unix::ffi::OsStrExt;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let rest: Vec<std::ffi::OsString> = args.collect();

    if let Some(path) = std::env::var_os("FAKE_REAL_TMUX_RECORD") {
        let env: serde_json::Map<String, serde_json::Value> = std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    serde_json::Value::String(v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let line = serde_json::json!({
            "argv0": hex(argv0.as_bytes()),
            "args": rest.iter().map(|a| hex(a.as_bytes())).collect::<Vec<_>>(),
            "env": env,
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }

    if rest.len() == 1 && rest[0] == "-V" {
        println!("tmux 9.9-fake");
    }
    let code = std::env::var("FAKE_REAL_TMUX_EXIT")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);
    std::process::exit(code);
}
