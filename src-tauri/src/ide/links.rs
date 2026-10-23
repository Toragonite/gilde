//! `link_open` (contract §4.2.5, §9.1) [F-39].
//!
//! Owner: T11.
//!
//! The request rule (http/https, no userinfo, ≤ 2048 chars) is the contract's
//! own (`is_openable_url`, checked by `Req` before anything here runs). Then:
//!
//! * the host is classified **after IP parsing, without resolving names**:
//!   loopback, link-local, unspecified, RFC 1918 private and IPv6 ULA
//!   addresses (IPv4-mapped IPv6 judged as their IPv4), `localhost` /
//!   `*.localhost` and `*.local` names are **local**;
//! * a local link opens only after a **native** confirmation naming the
//!   address (`link.localConfirm`, §9.12) — the WebView cannot answer it; a
//!   declined (or unavailable) confirmation is `opened: false`;
//! * the URL is opened by `NSWorkspace openURL:` in its normalized form (the
//!   `url` crate's serialization: punycode hosts, percent-encoding); the
//!   WebView never navigates.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::error::IpcError;

/// Where a link points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostClass {
    Public,
    /// A local address; the string names it for the confirmation
    /// (`host[:port]`, IPv6 in brackets).
    Local(String),
}

fn ipv4_local(a: Ipv4Addr) -> bool {
    a.is_loopback() || a.is_link_local() || a.is_private() || a.is_unspecified() || a.is_broadcast()
}

fn ipv6_local(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return ipv4_local(v4);
    }
    let seg0 = a.segments()[0];
    a.is_loopback()
        || a.is_unspecified()
        // fe80::/10 link-local
        || (seg0 & 0xffc0) == 0xfe80
        // fc00::/7 unique local
        || (seg0 & 0xfe00) == 0xfc00
}

/// Whether an IP address is local (§4.2.5).
pub fn ip_is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => ipv4_local(a),
        IpAddr::V6(a) => ipv6_local(a),
    }
}

/// The normalized URL and its host class; `INVALID_ARGUMENT{url}` when it
/// does not parse as an openable http(s) URL.
pub fn classify(raw: &str) -> Result<(url::Url, HostClass), IpcError> {
    if !crate::contract::ipc::changes::is_openable_url(raw) {
        return Err(IpcError::invalid_argument("url", "url"));
    }
    let u = url::Url::parse(raw).map_err(|_| IpcError::invalid_argument("url", "url"))?;
    let local = match u.host() {
        Some(url::Host::Ipv4(a)) => ipv4_local(a),
        Some(url::Host::Ipv6(a)) => ipv6_local(a),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local")
        }
        None => return Err(IpcError::invalid_argument("url", "url")),
    };
    let class = if local {
        let host = match u.host() {
            Some(url::Host::Ipv6(a)) => format!("[{a}]"),
            _ => u.host_str().unwrap_or_default().to_string(),
        };
        let named = match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        };
        HostClass::Local(named)
    } else {
        HostClass::Public
    };
    Ok((u, class))
}

/// The native confirmation for a local address (`link.localConfirm`).
#[async_trait]
pub trait LinkConfirm: Send + Sync {
    /// `true` when the user chose to open `host`.
    async fn confirm_local(&self, host: &str) -> bool;
}

/// The fail-safe confirmation: declines every local link (used when no
/// native dialog is available).
pub struct DeclineLocal;

#[async_trait]
impl LinkConfirm for DeclineLocal {
    async fn confirm_local(&self, _host: &str) -> bool {
        false
    }
}

/// Opens a URL in the user's browser.
pub trait UrlOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), IpcError>;
}

/// `NSWorkspace openURL:` (no subprocess, no WebView navigation).
pub struct WorkspaceOpener;

impl UrlOpener for WorkspaceOpener {
    fn open(&self, url: &str) -> Result<(), IpcError> {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::{NSString, NSURL};
        let u = NSURL::URLWithString(&NSString::from_str(url)).ok_or_else(|| IpcError::invalid_argument("url", "url"))?;
        if NSWorkspace::sharedWorkspace().openURL(&u) {
            Ok(())
        } else {
            Err(IpcError::spawn_failed())
        }
    }
}

/// `link_open`: classify, confirm a local address natively, open (on the
/// blocking pool: LaunchServices may take a moment).
pub async fn open_link(raw: &str, confirm: &dyn LinkConfirm, opener: Arc<dyn UrlOpener>) -> Result<bool, IpcError> {
    let (u, class) = classify(raw)?;
    if let HostClass::Local(host) = &class {
        if !confirm.confirm_local(host).await {
            return Ok(false);
        }
    }
    let url = u.as_str().to_string();
    tokio::task::spawn_blocking(move || opener.open(&url))
        .await
        .map_err(|_| IpcError::internal("link-open"))??;
    Ok(true)
}

/// The confirmation's text (§9.12 `link.localConfirm`): `(message, open, cancel)`.
pub fn confirm_text(korean: bool, host: &str) -> (String, &'static str, &'static str) {
    if korean {
        (format!("이 링크는 로컬 주소({host})를 가리킵니다. 그래도 열 수 있습니다."), "열기", "취소")
    } else {
        (format!("This link points to a local address ({host}). Open it anyway?"), "Open", "Cancel")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(u: &str) -> HostClass {
        classify(u).unwrap().1
    }

    #[test]
    fn local_addresses_after_ip_parsing() {
        for (u, named) in [
            ("http://127.0.0.1:8080/x", "127.0.0.1:8080"),
            ("http://localhost/", "localhost"),
            ("https://api.localhost/", "api.localhost"),
            ("http://printer.local./", "printer.local."),
            ("http://10.1.2.3/", "10.1.2.3"),
            ("http://172.16.0.1/", "172.16.0.1"),
            ("http://192.168.1.1/admin", "192.168.1.1"),
            ("http://169.254.169.254/latest/meta-data", "169.254.169.254"),
            ("http://0.0.0.0/", "0.0.0.0"),
            ("http://[::1]:3000/", "[::1]:3000"),
            ("http://[fe80::1]/", "[fe80::1]"),
            ("http://[fd12:3456::1]/", "[fd12:3456::1]"),
            ("http://[::ffff:192.168.0.1]/", "[::ffff:192.168.0.1]"),
            // Numeric spellings the URL parser turns into IPv4.
            ("http://2130706433/", "127.0.0.1"),
            ("http://0x7f000001/", "127.0.0.1"),
            ("http://127.1/", "127.0.0.1"),
        ] {
            assert_eq!(class(u), HostClass::Local(named.to_string()), "{u}");
        }
    }

    #[test]
    fn public_hosts_are_not_confirmed_and_names_are_not_resolved() {
        for u in [
            "https://example.com/",
            "http://8.8.8.8/",
            "http://[2001:db8::1]/",
            "http://172.32.0.1/",
            // A name that may resolve to a local address is not resolved.
            "http://localtest.me/",
            "http://local.example.com/",
        ] {
            assert_eq!(class(u), HostClass::Public, "{u}");
        }
    }

    #[test]
    fn the_request_rule_comes_first() {
        for u in ["javascript:alert(1)", "file:///etc/passwd", "https://u:p@example.com/", "ftp://example.com/", ""] {
            let e = classify(u).unwrap_err();
            assert_eq!((e.params["field"].as_str(), e.params["rule"].as_str()), (Some("url"), Some("url")), "{u}");
        }
    }

    struct Answer(bool, std::sync::Mutex<Vec<String>>);
    #[async_trait]
    impl LinkConfirm for Answer {
        async fn confirm_local(&self, host: &str) -> bool {
            self.1.lock().unwrap().push(host.to_string());
            self.0
        }
    }
    #[derive(Default)]
    struct Opened(std::sync::Mutex<Vec<String>>);
    impl UrlOpener for Opened {
        fn open(&self, url: &str) -> Result<(), IpcError> {
            self.0.lock().unwrap().push(url.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_local_link_opens_only_after_the_native_confirmation() {
        let opened = Arc::new(Opened::default());
        let no = Answer(false, Default::default());
        assert!(!open_link("http://127.0.0.1:5173/", &no, opened.clone()).await.unwrap());
        assert_eq!(no.1.lock().unwrap().as_slice(), ["127.0.0.1:5173".to_string()]);
        assert!(opened.0.lock().unwrap().is_empty(), "declined → nothing opened");

        let yes = Answer(true, Default::default());
        assert!(open_link("http://127.0.0.1:5173/", &yes, opened.clone()).await.unwrap());
        assert!(open_link("https://EXAMPLE.com/a b", &yes, opened.clone()).await.unwrap());
        assert_eq!(yes.1.lock().unwrap().len(), 1, "a public link asks nothing");
        let urls = opened.0.lock().unwrap().clone();
        assert_eq!(urls, vec!["http://127.0.0.1:5173/".to_string(), "https://example.com/a%20b".to_string()]);
        assert!(!open_link("http://localhost/", &DeclineLocal, opened).await.unwrap());
    }

    #[test]
    fn the_confirmation_names_the_address_in_both_languages() {
        let (ko, open, cancel) = confirm_text(true, "127.0.0.1:8080");
        assert!(ko.contains("127.0.0.1:8080") && ko.ends_with("습니다."));
        assert_eq!((open, cancel), ("열기", "취소"));
        let (en, _, _) = confirm_text(false, "[::1]");
        assert!(en.contains("[::1]"));
    }
}
