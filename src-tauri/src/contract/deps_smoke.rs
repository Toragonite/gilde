//! Smoke test: every crate the contract pre-declares (§15.2 T01, F-17, N-08)
//! links and is usable from this crate, so wave-1 tasks never need to edit the
//! dependency manifests (H4).

#[test]
fn every_predeclared_crate_is_usable() {
    // ts-rs: a derive is visible through its trait.
    use ts_rs::TS as _;
    assert_eq!(crate::contract::common::Vendor::name(&ts_rs::Config::default()), "Vendor");

    // async-trait: an object-safe async trait compiles.
    #[async_trait::async_trait]
    trait Probe: Send + Sync {
        async fn ping(&self) -> u8;
    }
    struct P;
    #[async_trait::async_trait]
    impl Probe for P {
        async fn ping(&self) -> u8 {
            7
        }
    }
    let boxed: Box<dyn Probe> = Box::new(P);
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert_eq!(rt.block_on(boxed.ping()), 7);

    // zeroize
    use zeroize::Zeroize as _;
    let mut secret = vec![1u8, 2, 3];
    secret.zeroize();
    assert!(secret.is_empty());

    // objc2 family (types only; nothing touches AppKit in a unit test).
    let _sel = objc2::sel!(description);
    let _ = std::any::type_name::<objc2_foundation::NSString>();
    let _ = std::any::type_name::<objc2_app_kit::NSWorkspace>();
    let _ = std::any::type_name::<objc2_app_kit::NSRunningApplication>();
    let _ = std::any::type_name::<objc2_user_notifications::UNUserNotificationCenter>();

    // unicode-segmentation / -width / -normalization
    use unicode_normalization::UnicodeNormalization as _;
    use unicode_segmentation::UnicodeSegmentation as _;
    assert_eq!("한국어".graphemes(true).count(), 3);
    assert_eq!(unicode_width::UnicodeWidthStr::width("한"), 2);
    let nfd = "\u{1100}\u{1161}"; // ᄀ + ᅡ
    assert_eq!(nfd.nfc().collect::<String>(), "가");

    // md-5 (cursor chat dirs are md5(cwd), §5.4)
    use md5::Digest as _;
    let d = md5::Md5::digest(b"");
    assert_eq!(hex::encode(d), "d41d8cd98f00b204e9800998ecf8427e");

    // memchr, url, aho-corasick
    assert_eq!(memchr::memchr(b'\n', b"ab\ncd"), Some(2));
    assert_eq!(url::Url::parse("https://example.com").unwrap().host_str(), Some("example.com"));
    let ac = aho_corasick::AhoCorasick::new(["can_use_tool", "control_request"]).unwrap();
    assert_eq!(ac.find_iter("x can_use_tool y control_request").count(), 2);

    // nix features: socket, user, event (kqueue), signal, process.
    let _ = nix::sys::socket::AddressFamily::Unix;
    let _ = nix::unistd::Uid::current();
    let _ = nix::sys::event::EventFilter::EVFILT_PROC;
    let _ = nix::sys::signal::Signal::SIGTERM;
    let _ = nix::unistd::getpid();

    // proptest (dev): a trivial strategy runs.
    use proptest::strategy::{Strategy as _, ValueTree as _};
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let v = (0u8..10).new_tree(&mut runner).unwrap().current();
    assert!(v < 10);
}
