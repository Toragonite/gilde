//! Value-level validation of IPC requests (contract §3.1 rule 4, §4.1 rule 3, §9.9).
//!
//! Every request reaches its command through [`Req<T>`], which performs, in
//! this order and before anything else happens (§4.1 rule 3 (a)–(b)):
//!
//! 1. the **envelope** check: the invoke arguments are a JSON object whose keys
//!    are exactly `req` (plus `channel` for the three streaming commands);
//! 2. the **size guard**: the serialized `req` is at most the command's body cap
//!    (manifest `max_body_bytes`: 1 MiB, `pty_write` 1.4 MiB, `turn_send`
//!    300 KiB) and nests at most [`MAX_JSON_DEPTH`] levels;
//! 3. **deserialization** with `deny_unknown_fields` → any failure (wrong type,
//!    missing required field, unknown field, unknown enum variant) is
//!    `INVALID_ARGUMENT{field: "$", rule: "shape"}`;
//! 4. [`Validate::validate`]: the value rules of §3.2 and §4.2, reported as
//!    `INVALID_ARGUMENT{field: <json path>, rule: <rule id>}`;
//! 5. the rate class (§4.1 rule 6, A45.3, [`crate::ipc_rate::admit`]): a valid
//!    request draws a token from its class's bucket, or is `BUSY{retryAfterMs}`.
//!
//! Only then does the command body run, so a rejected request never has a side
//! effect (and a request refused at 1–4 draws no token). Absent vs invalid (§3.1 rule 4): an absent optional field, an empty
//! optional string or an empty optional list means "not requested"; a present
//! value that breaks its rule is invalid; a **required** string that is empty is
//! invalid (`rule: "required"`).
//!
//! ## Rule ids
//!
//! | id | meaning |
//! |---|---|
//! | `shape` | not deserializable into the request type (contract §4.1) |
//! | `envelope` | invoke args are not `{ req }` / `{ req, channel }` |
//! | `size` | serialized request over the command's body cap |
//! | `depth` | JSON nesting deeper than allowed (64, or 16 for `ui_state_set.value`) |
//! | `required` | a required string is empty |
//! | `maxLength` / `minLength` | string length (UTF-8 bytes unless the rule says chars) |
//! | `maxItems` / `minItems` | list length |
//! | `range` | a number outside its stated range |
//! | `pattern` | an id or value that does not match its §3.2 / §4.2 pattern |
//! | `nul` | contains a NUL character |
//! | `control` | contains a control character where none is allowed |
//! | `unique` | a list holds the same value twice |
//! | `path` | not a valid relative path (no `..`, no NUL, not absolute); for an `AbsPath`, a control, bidi or zero-width character (A45.1) |
//! | `absPath` | not an absolute, canonical path (§3.4.1) |
//! | `url` | `link_open`: not http/https, has userinfo, or over 2048 chars |
//! | `reserved` | `envPassthrough` names a reserved variable (§6.0) |
//! | `kindForCard` | decision kind does not fit the card kind (contract §4.2.3) |
//!
//! Field paths use the JSON (camelCase) names, `.` between members and `[i]`
//! for list items, e.g. `decision.answers[0].labels[1]`. A violation of the
//! whole request uses the root path `$`.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::contract::error::IpcError;
use crate::contract::ipc_manifest;

/// JSON nesting allowed in a request (§4.1 rule 7).
pub const MAX_JSON_DEPTH: usize = 64;
/// Default cap for a string field "unless stated" (§4.1 rule 7): 64 KiB.
pub const DEFAULT_MAX_STRING_BYTES: usize = 64 * 1024;
/// Default cap for a list field "unless stated" (§4.1 rule 7).
pub const DEFAULT_MAX_ITEMS: usize = 1_000;
/// The root path used for whole-request violations.
pub const ROOT: &str = "$";

/// One failed value rule: where and which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// JSON path of the offending value (`$` for the whole request).
    pub field: String,
    /// The rule id (see the module docs).
    pub rule: &'static str,
}

impl Invalid {
    pub fn new(field: impl Into<String>, rule: &'static str) -> Invalid {
        Invalid {
            field: field.into(),
            rule,
        }
    }

    /// Prefix this violation's path with `parent` (`parent.field`).
    pub fn under(mut self, parent: &str) -> Invalid {
        if parent.is_empty() || parent == ROOT {
            return self;
        }
        self.field = if self.field.is_empty() || self.field == ROOT {
            parent.to_string()
        } else if self.field.starts_with('[') {
            format!("{parent}{}", self.field)
        } else {
            format!("{parent}.{}", self.field)
        };
        self
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.field, self.rule)
    }
}

impl From<Invalid> for IpcError {
    fn from(v: Invalid) -> IpcError {
        IpcError::invalid_argument(&v.field, v.rule)
    }
}

/// Result of a value check.
pub type Checked = Result<(), Invalid>;

/// Value rules of a contract type. Implemented by every request type and by
/// every type nested in a request. The default accepts (types whose shape is
/// their whole rule, e.g. closed enums).
pub trait Validate {
    fn validate(&self) -> Checked {
        Ok(())
    }
}

impl<T: Validate> Validate for Option<T> {
    fn validate(&self) -> Checked {
        match self {
            Some(v) => v.validate(),
            None => Ok(()),
        }
    }
}

impl<T: Validate> Validate for Box<T> {
    fn validate(&self) -> Checked {
        (**self).validate()
    }
}

/// Validate `value` and report violations under `field`.
pub fn nested<T: Validate + ?Sized>(field: &str, value: &T) -> Checked {
    value.validate().map_err(|e| e.under(field))
}

/// Validate every item of a list (with the default item cap) under `field[i]`.
pub fn each<T: Validate>(field: &str, items: &[T]) -> Checked {
    max_items(field, items, DEFAULT_MAX_ITEMS)?;
    for (i, item) in items.iter().enumerate() {
        item.validate().map_err(|e| e.under(&format!("{field}[{i}]")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// string rules
// ---------------------------------------------------------------------------

/// A required string: non-empty, no NUL, at most `max` UTF-8 bytes.
pub fn required_str(field: &str, s: &str, max: usize) -> Checked {
    if s.is_empty() {
        return Err(Invalid::new(field, "required"));
    }
    text(field, s, max)
}

/// Any string: no NUL, at most `max` UTF-8 bytes (empty allowed).
pub fn text(field: &str, s: &str, max: usize) -> Checked {
    if s.len() > max {
        return Err(Invalid::new(field, "maxLength"));
    }
    if s.contains('\0') {
        return Err(Invalid::new(field, "nul"));
    }
    Ok(())
}

/// A string whose length is bounded in **characters** (Unicode scalar values)
/// rather than bytes, e.g. "1..=64 chars". `min` 0 allows empty.
pub fn chars(field: &str, s: &str, min: usize, max: usize) -> Checked {
    let n = s.chars().count();
    if n < min {
        return Err(Invalid::new(field, if min == 1 && n == 0 { "required" } else { "minLength" }));
    }
    if n > max {
        return Err(Invalid::new(field, "maxLength"));
    }
    if s.contains('\0') {
        return Err(Invalid::new(field, "nul"));
    }
    Ok(())
}

/// Text with a byte range `min..=max` (e.g. `text 1..=256 KiB`).
pub fn text_range(field: &str, s: &str, min: usize, max: usize) -> Checked {
    if s.len() < min {
        return Err(Invalid::new(field, if s.is_empty() { "required" } else { "minLength" }));
    }
    text(field, s, max)
}

/// An optional string where `None` and `""` both mean "not requested"; a
/// present non-empty value must satisfy `rule`.
pub fn optional_str(field: &str, s: &Option<String>, rule: impl FnOnce(&str, &str) -> Checked) -> Checked {
    match s.as_deref() {
        None | Some("") => Ok(()),
        Some(v) => rule(field, v),
    }
}

/// No C0/C1 control characters at all (ids, labels, single-line names).
pub fn no_controls(field: &str, s: &str) -> Checked {
    if s.chars().any(|c| c.is_control()) {
        return Err(Invalid::new(field, "control"));
    }
    Ok(())
}

/// A relative path as the WebView may send it (§3.1 rule 7, §9.4): non-empty,
/// ≤ 1024 bytes, not absolute, no NUL, no `..` component.
pub fn rel_path(field: &str, s: &str) -> Checked {
    required_str(field, s, 1024)?;
    if s.starts_with('/') || s.split('/').any(|c| c == "..") {
        return Err(Invalid::new(field, "path"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// numbers and lists
// ---------------------------------------------------------------------------

/// `min <= v <= max`.
pub fn range<N: PartialOrd + Copy>(field: &str, v: N, min: N, max: N) -> Checked {
    if v < min || v > max {
        return Err(Invalid::new(field, "range"));
    }
    Ok(())
}

pub fn max_items<T>(field: &str, items: &[T], max: usize) -> Checked {
    if items.len() > max {
        return Err(Invalid::new(field, "maxItems"));
    }
    Ok(())
}

pub fn items_range<T>(field: &str, items: &[T], min: usize, max: usize) -> Checked {
    if items.len() < min {
        return Err(Invalid::new(field, "minItems"));
    }
    max_items(field, items, max)
}

pub fn unique<T: PartialEq>(field: &str, items: &[T]) -> Checked {
    for (i, a) in items.iter().enumerate() {
        if items[..i].iter().any(|b| b == a) {
            return Err(Invalid::new(format!("{field}[{i}]"), "unique"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON shape guards (before deserialization)
// ---------------------------------------------------------------------------

/// Nesting depth of a JSON value (a scalar is depth 0, `{}`/`[]` depth 1).
pub fn json_depth(v: &Value) -> usize {
    // Iterative, so a hostile value cannot overflow our own stack.
    let mut max = 0;
    let mut stack: Vec<(&Value, usize)> = vec![(v, 0)];
    while let Some((node, d)) = stack.pop() {
        match node {
            Value::Array(items) => {
                let d = d + 1;
                max = max.max(d);
                stack.extend(items.iter().map(|i| (i, d)));
            }
            Value::Object(map) => {
                let d = d + 1;
                max = max.max(d);
                stack.extend(map.values().map(|i| (i, d)));
            }
            _ => {}
        }
    }
    max
}

/// Length in bytes of `v` serialized as compact JSON, without allocating it.
pub fn json_len(v: &Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counter(0);
    // Writing a `Value` into an infallible sink cannot fail.
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}

/// The first place where `input` and `reparsed` (the re-serialized request)
/// disagree in structure, as a JSON path. Two serde leniencies would otherwise
/// let a malformed request through:
///
/// * `deny_unknown_fields` does not cover the unit variants of internally
///   tagged enums (`{"kind":"deny","x":1}` is accepted) — any key the type
///   silently dropped is reported;
/// * a struct also deserializes from a positional JSON array (`[]`,
///   `[null, …]`) — an array where the type serializes an object (or the
///   reverse) is reported.
pub fn ignored_key(input: &Value, reparsed: &Value, path: &str) -> Option<String> {
    let here = || Some(if path.is_empty() { ROOT.to_string() } else { path.to_string() });
    match (input, reparsed) {
        (Value::Object(a), Value::Object(b)) => a.iter().find_map(|(k, v)| {
            let child = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
            match b.get(k) {
                None => Some(child),
                Some(w) => ignored_key(v, w, &child),
            }
        }),
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return here();
            }
            a.iter()
                .zip(b.iter())
                .enumerate()
                .find_map(|(i, (v, w))| ignored_key(v, w, &format!("{path}[{i}]")))
        }
        (Value::Array(_), Value::Object(_)) | (Value::Object(_), Value::Array(_)) => here(),
        _ => None,
    }
}

/// Steps (a) and (b) of §4.1 rule 3 for one already-extracted `req` value.
///
/// `max_body` is the command's byte cap; the caller passes the manifest value.
pub fn decode_request<T>(req: &Value, max_body: usize) -> Result<T, IpcError>
where
    T: DeserializeOwned + Serialize + Validate,
{
    if json_len(req) > max_body {
        return Err(IpcError::invalid_argument(ROOT, "size"));
    }
    if json_depth(req) > MAX_JSON_DEPTH {
        return Err(IpcError::invalid_argument(ROOT, "depth"));
    }
    let shape = || IpcError::invalid_argument(ROOT, "shape");
    let parsed: T = serde_json::from_value(req.clone()).map_err(|_| shape())?;
    let reparsed = serde_json::to_value(&parsed).map_err(|_| shape())?;
    if ignored_key(req, &reparsed, "").is_some() {
        return Err(shape());
    }
    parsed.validate()?;
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// the command argument wrapper
// ---------------------------------------------------------------------------

/// A validated request. Every command takes its request as `req: Req<XReq>`;
/// the WebView sends `invoke("x", { req })` (plus `channel`), exactly as for a
/// plain `req: XReq` — the wrapper only changes *how* a bad request is refused:
/// always with the one [`IpcError`] shape, never with Tauri's own string.
pub struct Req<T>(T);

impl<T> Req<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for Req<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Req<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Req").field(&self.0).finish()
    }
}

/// Extract and validate the `req` argument of the command `name` from the raw
/// invoke payload. Pure (no Tauri types), so tests and the fuzzer can drive it.
pub fn request_from_payload<T>(name: &str, payload: &Value) -> Result<T, IpcError>
where
    T: DeserializeOwned + Serialize + Validate,
{
    let spec = ipc_manifest::command(name).ok_or_else(|| IpcError::invalid_argument(ROOT, "envelope"))?;
    let Value::Object(args) = payload else {
        return Err(IpcError::invalid_argument(ROOT, "envelope"));
    };
    let allowed = |k: &str| k == "req" || (k == "channel" && spec.channel.is_some());
    if args.keys().any(|k| !allowed(k)) {
        return Err(IpcError::invalid_argument(ROOT, "envelope"));
    }
    let req = args.get("req").ok_or_else(|| IpcError::invalid_argument(ROOT, "shape"))?;
    decode_request(req, spec.max_body_bytes)
}

impl<'de, R, T> tauri::ipc::CommandArg<'de, R> for Req<T>
where
    R: tauri::Runtime,
    T: DeserializeOwned + Serialize + Validate,
{
    fn from_command(command: tauri::ipc::CommandItem<'de, R>) -> Result<Self, tauri::ipc::InvokeError> {
        let result = match command.message.payload() {
            tauri::ipc::InvokeBody::Json(payload) => request_from_payload::<T>(command.name, payload).and_then(|req| {
                // §4.1 rule 6 (A45.3): a valid request draws from its class's bucket.
                let raw = payload.get("req").unwrap_or(&Value::Null);
                crate::ipc_rate::admit(command.message.webview_ref(), command.name, raw)?;
                Ok(req)
            }),
            tauri::ipc::InvokeBody::Raw(_) => Err(IpcError::invalid_argument(ROOT, "envelope")),
        };
        result.map(Req).map_err(tauri::ipc::InvokeError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn depth_counts_containers_only() {
        assert_eq!(json_depth(&json!(1)), 0);
        assert_eq!(json_depth(&json!({})), 1);
        assert_eq!(json_depth(&json!({"a": [[1]]})), 3);
    }

    #[test]
    fn json_len_matches_serialization() {
        let v = json!({"a": "héllo", "b": [1, 2, null]});
        assert_eq!(json_len(&v), serde_json::to_string(&v).unwrap().len());
    }

    #[test]
    fn ignored_keys_are_found_at_any_depth() {
        let input = json!({"a": {"kind": "deny", "x": 1}, "b": [{"k": 1, "z": 2}]});
        let back = json!({"a": {"kind": "deny"}, "b": [{"k": 1}]});
        assert_eq!(ignored_key(&input, &back, "").as_deref(), Some("a.x"));
        let back2 = json!({"a": {"kind": "deny", "x": 1}, "b": [{"k": 1}]});
        assert_eq!(ignored_key(&input, &back2, "").as_deref(), Some("b[0].z"));
        assert_eq!(ignored_key(&input, &input, ""), None);
        assert_eq!(ignored_key(&json!([]), &json!({}), "").as_deref(), Some("$"));
        assert_eq!(ignored_key(&json!({"f": [null]}), &json!({"f": {"a": null}}), "").as_deref(), Some("f"));
    }

    #[test]
    fn paths_nest_with_dots_and_brackets() {
        let e = Invalid::new("labels[1]", "maxLength").under("answers[0]").under("decision");
        assert_eq!(e.field, "decision.answers[0].labels[1]");
        let root = Invalid::new(ROOT, "shape").under("x");
        assert_eq!(root.field, "x");
    }

    #[test]
    fn absent_empty_and_invalid_are_kept_apart() {
        let rule = |f: &str, v: &str| text_range(f, v, 1, 3);
        assert!(optional_str("m", &None, rule).is_ok());
        assert!(optional_str("m", &Some(String::new()), rule).is_ok());
        assert_eq!(optional_str("m", &Some("abcd".into()), rule).unwrap_err().rule, "maxLength");
        assert_eq!(required_str("p", "", 10).unwrap_err().rule, "required");
        assert_eq!(text("p", "a\0b", 10).unwrap_err().rule, "nul");
    }
}
