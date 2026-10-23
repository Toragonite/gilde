//! Identity resolution, merges and rekeys (§3.3).
//!
//! Owner: T02. World is the only place that resolves an [`IdentityHint`] to a
//! [`SessionUid`]. Rules (§3.3.2, first match wins):
//! 1. a known `gildeSpawnId` → that Gilde session;
//! 2. `vendorSessionId` + `accountId` → the uid of that [`IdentityKey`];
//! 3. `vendorSessionId` without an account (teammates) → through the lead's
//!    account; unknown lead → held ≤ 10 s by the caller, then filed under
//!    account `None`;
//! 4. `process` → the uid holding that `ProcessKey`;
//! 5. otherwise a new uid (persisted only once a vendor id is known).
//!
//! Binding (§3.3.3): a provisional session that learns its vendor id is
//! *attached* (same uid). If that key already belongs to another uid, the
//! **older** uid (persisted first; else minted first — UUIDv7 sorts by time)
//! survives and the other becomes a tombstone (`mergedInto`) for 10 min. A
//! uid is never reused for a different key: a hint that brings a second key
//! for a keyed uid *rekeys* the process to a new uid.

use std::collections::HashMap;

use crate::contract::common::{EpochMs, Vendor};
use crate::contract::ids::{AccountId, ProcessKey, SessionUid};
use crate::contract::services::{IdentityHint, IdentityKey};

/// Tombstones are removed from the store after 10 min (§3.3.3).
pub const TOMBSTONE_MS: u64 = 600_000;
/// Rule 3: an update for a teammate whose lead is unknown is held ≤ 10 s.
pub const TEAMMATE_HOLD_MS: u64 = 10_000;

/// The outcome of [`IdentityIndex::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The session the hint belongs to (`rule` 1–5; `minted` for rule 5 and
    /// for rule 2/3 keys seen for the first time).
    Uid { uid: SessionUid, rule: u8, minted: bool, account: Option<AccountId> },
    /// Rule 3 with an unknown lead: hold the update (the caller retries).
    Hold,
}

/// What [`IdentityIndex::bind`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindOutcome {
    /// The uid the update's facts belong to after binding.
    pub uid: Option<SessionUid>,
    /// `(survivor, tombstone)` of a merge.
    pub merged: Option<(SessionUid, SessionUid)>,
    /// `(old, new)`: the process moved to a new uid for a new key.
    pub rekeyed: Option<(SessionUid, SessionUid)>,
    /// The uid got its vendor key now (persist it).
    pub newly_keyed: bool,
}

/// The identity maps.
#[derive(Debug, Default)]
pub struct IdentityIndex {
    by_key: HashMap<IdentityKey, SessionUid>,
    key_of: HashMap<SessionUid, IdentityKey>,
    by_spawn: HashMap<String, SessionUid>,
    by_process: HashMap<ProcessKey, SessionUid>,
    process_of: HashMap<SessionUid, ProcessKey>,
    /// When a uid was first persisted (`session_meta.first_seen_at`).
    persisted_at: HashMap<SessionUid, EpochMs>,
    /// tombstone → (survivor, expiry on the uptime clock)
    tombstones: HashMap<SessionUid, (SessionUid, u64)>,
}

impl IdentityIndex {
    /// Load a persisted `session_meta` row (boot).
    pub fn load(&mut self, uid: SessionUid, key: Option<IdentityKey>, first_seen_at: EpochMs) {
        if let Some(k) = key {
            self.by_key.insert(k.clone(), uid.clone());
            self.key_of.insert(uid.clone(), k);
        }
        self.persisted_at.insert(uid, first_seen_at);
    }

    /// Follow tombstones to the surviving uid.
    pub fn live(&self, uid: &SessionUid) -> SessionUid {
        let mut u = uid.clone();
        for _ in 0..8 {
            match self.tombstones.get(&u) {
                Some((into, _)) => u = into.clone(),
                None => break,
            }
        }
        u
    }

    pub fn key_of(&self, uid: &SessionUid) -> Option<&IdentityKey> {
        self.key_of.get(uid)
    }

    pub fn uid_of_key(&self, key: &IdentityKey) -> Option<SessionUid> {
        self.by_key.get(key).map(|u| self.live(u))
    }

    pub fn uid_of_spawn(&self, spawn_id: &str) -> Option<SessionUid> {
        self.by_spawn.get(spawn_id).map(|u| self.live(u))
    }

    pub fn uid_of_process(&self, p: &ProcessKey) -> Option<SessionUid> {
        self.by_process.get(p).map(|u| self.live(u))
    }

    pub fn process_of(&self, uid: &SessionUid) -> Option<&ProcessKey> {
        self.process_of.get(uid)
    }

    pub fn is_persisted(&self, uid: &SessionUid) -> bool {
        self.persisted_at.contains_key(uid)
    }

    pub fn mark_persisted(&mut self, uid: &SessionUid, at: EpochMs) {
        self.persisted_at.entry(uid.clone()).or_insert(at);
    }

    pub fn is_tombstone(&self, uid: &SessionUid) -> bool {
        self.tombstones.contains_key(uid)
    }

    /// Every key whose vendor and session id match, any account.
    fn keys_for_vsid(&self, vendor: Vendor, vsid: &str) -> Vec<&IdentityKey> {
        self.by_key.keys().filter(|k| k.vendor == vendor && k.vendor_session_id == vsid).collect()
    }

    /// The account of a lead whose vendor session id is known (rule 3).
    pub fn account_of_vsid(&self, vendor: Vendor, vsid: &str) -> Option<AccountId> {
        let with_account: Vec<&IdentityKey> =
            self.keys_for_vsid(vendor, vsid).into_iter().filter(|k| k.account_id.is_some()).collect();
        match with_account.as_slice() {
            [k] => k.account_id.clone(),
            _ => None,
        }
    }

    /// The uid of a key, minting one if the key is new (history rows,
    /// `UidResolver::uid_for`, `--session-id` starts).
    pub fn uid_for_key(&mut self, key: &IdentityKey) -> (SessionUid, bool) {
        if let Some(u) = self.uid_of_key(key) {
            return (u, false);
        }
        let uid = SessionUid::mint();
        self.by_key.insert(key.clone(), uid.clone());
        self.key_of.insert(uid.clone(), key.clone());
        (uid, true)
    }

    /// Bind a Gilde spawn id to a uid (minting one when `uid` is `None`).
    pub fn bind_spawn(&mut self, spawn_id: &str, uid: Option<&SessionUid>) -> SessionUid {
        if let Some(u) = self.uid_of_spawn(spawn_id) {
            return u;
        }
        let u = uid.map(|u| self.live(u)).unwrap_or_else(SessionUid::mint);
        self.by_spawn.insert(spawn_id.to_string(), u.clone());
        u
    }

    /// A20.1: bind `spawn_id` to `uid`, always. Another uid already holding
    /// the spawn (an own-unbound row made by evidence that arrived before the
    /// bind) is absorbed: its spawn ids and process move to `uid`, its key
    /// moves when `uid` has none (else it is dropped), and it leaves every
    /// map. Returns the absorbed uid; the caller merges its record.
    pub fn bind_spawn_to(&mut self, spawn_id: &str, uid: &SessionUid) -> Option<SessionUid> {
        let uid = self.live(uid);
        let other = match self.uid_of_spawn(spawn_id) {
            Some(u) if u == uid => return None,
            Some(u) => u,
            None => {
                self.by_spawn.insert(spawn_id.to_string(), uid);
                return None;
            }
        };
        for v in self.by_spawn.values_mut() {
            if *v == other {
                *v = uid.clone();
            }
        }
        self.by_spawn.insert(spawn_id.to_string(), uid.clone());
        if let Some(p) = self.process_of.remove(&other) {
            if self.by_process.get(&p) == Some(&other) {
                self.by_process.remove(&p);
            }
            if let Some(old) = self.process_of.insert(uid.clone(), p.clone()) {
                if old != p && self.by_process.get(&old) == Some(&uid) {
                    self.by_process.remove(&old);
                }
            }
            self.by_process.insert(p, uid.clone());
        }
        if let Some(k) = self.key_of.remove(&other) {
            if self.by_key.get(&k) == Some(&other) {
                self.by_key.remove(&k);
            }
            if !self.key_of.contains_key(&uid) {
                self.by_key.insert(k.clone(), uid.clone());
                self.key_of.insert(uid.clone(), k);
            }
        }
        if let Some(at) = self.persisted_at.remove(&other) {
            let e = self.persisted_at.entry(uid.clone()).or_insert(at);
            *e = (*e).min(at);
        }
        self.tombstones.remove(&other);
        Some(other)
    }

    /// Keep `gone` as a tombstone of `into` for 10 min (a row that was shown).
    pub fn add_tombstone(&mut self, gone: &SessionUid, into: &SessionUid, now_up: u64) {
        self.tombstones.insert(gone.clone(), (into.clone(), now_up + TOMBSTONE_MS));
    }

    /// Resolve a hint (§3.3.2). `lead_account` is the account of the
    /// teammate's lead when the caller could find it (rule 3); `hold_expired`
    /// files a held teammate update under account `None`.
    pub fn resolve(&self, hint: &IdentityHint, lead_account: Option<AccountId>, hold_expired: bool) -> Resolved {
        let vendor = hint.vendor.unwrap_or(Vendor::Claude);
        // 1.
        if let Some(sp) = &hint.gilde_spawn_id {
            if let Some(u) = self.uid_of_spawn(sp) {
                return Resolved::Uid { uid: u, rule: 1, minted: false, account: hint.account_id.clone() };
            }
        }
        if let Some(vsid) = &hint.vendor_session_id {
            // 2.
            if let Some(acct) = &hint.account_id {
                let key = IdentityKey { vendor, account_id: Some(acct.clone()), vendor_session_id: vsid.clone() };
                if let Some(u) = self.uid_of_key(&key) {
                    return Resolved::Uid { uid: u, rule: 2, minted: false, account: Some(acct.clone()) };
                }
            } else {
                // 3.
                if let Some(acct) = lead_account.or_else(|| self.account_of_vsid(vendor, vsid)) {
                    let key = IdentityKey { vendor, account_id: Some(acct.clone()), vendor_session_id: vsid.clone() };
                    if let Some(u) = self.uid_of_key(&key) {
                        return Resolved::Uid { uid: u, rule: 3, minted: false, account: Some(acct) };
                    }
                    if let Some(u) = self.process_uid(hint) {
                        return Resolved::Uid { uid: u, rule: 4, minted: false, account: Some(acct) };
                    }
                    return Resolved::Uid { uid: SessionUid::mint(), rule: 3, minted: true, account: Some(acct) };
                }
                let unknown = IdentityKey { vendor, account_id: None, vendor_session_id: vsid.clone() };
                if let Some(u) = self.uid_of_key(&unknown) {
                    return Resolved::Uid { uid: u, rule: 3, minted: false, account: None };
                }
                if !hold_expired {
                    return Resolved::Hold;
                }
            }
        }
        // 4.
        if let Some(u) = self.process_uid(hint) {
            return Resolved::Uid { uid: u, rule: 4, minted: false, account: hint.account_id.clone() };
        }
        // 5.
        Resolved::Uid { uid: SessionUid::mint(), rule: 5, minted: true, account: hint.account_id.clone() }
    }

    fn process_uid(&self, hint: &IdentityHint) -> Option<SessionUid> {
        hint.process.as_ref().and_then(|p| self.uid_of_process(p))
    }

    /// The older of two uids: persisted first, else minted first.
    fn older(&self, a: &SessionUid, b: &SessionUid) -> bool {
        match (self.persisted_at.get(a), self.persisted_at.get(b)) {
            (Some(x), Some(y)) => (x, a.as_str()) <= (y, b.as_str()),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => a.as_str() <= b.as_str(),
        }
    }

    /// Bind the hint's spawn id, process and vendor key to `uid` (§3.3.3).
    pub fn bind(
        &mut self,
        uid: &SessionUid,
        hint: &IdentityHint,
        account: Option<AccountId>,
        now_up: u64,
    ) -> BindOutcome {
        let vendor = hint.vendor.unwrap_or(Vendor::Claude);
        let mut uid = self.live(uid);
        let mut out = BindOutcome::default();
        // The vendor key first: it can move the facts to another uid.
        if let Some(vsid) = &hint.vendor_session_id {
            let key = IdentityKey { vendor, account_id: account.clone(), vendor_session_id: vsid.clone() };
            match (self.key_of.get(&uid).cloned(), self.uid_of_key(&key)) {
                (Some(own), _) if own == key => {}
                (Some(_own), existing) => {
                    // Never reuse a uid for a different key: the process
                    // (and the spawn) moves to the uid of the new key.
                    let to = existing.unwrap_or_else(|| {
                        let n = SessionUid::mint();
                        self.by_key.insert(key.clone(), n.clone());
                        self.key_of.insert(n.clone(), key.clone());
                        out.newly_keyed = true;
                        n
                    });
                    if let Some(p) = self.process_of.remove(&uid) {
                        self.by_process.insert(p.clone(), to.clone());
                        self.process_of.insert(to.clone(), p);
                    }
                    for v in self.by_spawn.values_mut() {
                        if *v == uid {
                            *v = to.clone();
                        }
                    }
                    out.rekeyed = Some((uid.clone(), to.clone()));
                    uid = to;
                }
                (None, Some(other)) if other != uid => {
                    let (survivor, gone) = if self.older(&other, &uid) { (other, uid.clone()) } else { (uid.clone(), other) };
                    self.merge(&survivor, &gone, now_up);
                    out.merged = Some((survivor.clone(), gone));
                    uid = survivor;
                }
                (None, Some(_)) => {}
                (None, None) => {
                    // Attach (the unknown-account key of a held teammate is
                    // replaced by the real one when it is learned).
                    if account.is_some() {
                        let unknown = IdentityKey { vendor, account_id: None, vendor_session_id: vsid.clone() };
                        if self.by_key.get(&unknown) == Some(&uid) {
                            self.by_key.remove(&unknown);
                        }
                    }
                    self.by_key.insert(key.clone(), uid.clone());
                    self.key_of.insert(uid.clone(), key);
                    out.newly_keyed = true;
                }
            }
        }
        if let Some(sp) = &hint.gilde_spawn_id {
            self.by_spawn.entry(sp.clone()).or_insert_with(|| uid.clone());
        }
        if let Some(p) = &hint.process {
            match self.uid_of_process(p) {
                Some(other) if other != uid => {
                    if self.key_of.contains_key(&other) && self.key_of.contains_key(&uid) {
                        // Two keyed sessions: the process now serves `uid`.
                        self.process_of.remove(&other);
                        self.by_process.insert(p.clone(), uid.clone());
                        self.process_of.insert(uid.clone(), p.clone());
                    } else {
                        let (survivor, gone) =
                            if self.key_of.contains_key(&uid) || self.older(&uid, &other) { (uid.clone(), other) } else { (other, uid.clone()) };
                        self.merge(&survivor, &gone, now_up);
                        out.merged = Some((survivor.clone(), gone));
                        uid = survivor;
                    }
                }
                Some(_) => {}
                None => {
                    if let Some(old) = self.process_of.insert(uid.clone(), p.clone()) {
                        if &old != p {
                            self.by_process.remove(&old);
                        }
                    }
                    self.by_process.insert(p.clone(), uid.clone());
                }
            }
        }
        out.uid = Some(uid);
        out
    }

    /// Merge `gone` into `survivor`: every binding moves, `gone` becomes a
    /// tombstone for 10 min.
    fn merge(&mut self, survivor: &SessionUid, gone: &SessionUid, now_up: u64) {
        if survivor == gone {
            return;
        }
        if let Some(k) = self.key_of.remove(gone) {
            if !self.key_of.contains_key(survivor) {
                self.key_of.insert(survivor.clone(), k.clone());
            }
            self.by_key.insert(k, survivor.clone());
        }
        if let Some(p) = self.process_of.remove(gone) {
            if !self.process_of.contains_key(survivor) {
                self.by_process.insert(p.clone(), survivor.clone());
                self.process_of.insert(survivor.clone(), p);
            }
        }
        for v in self.by_spawn.values_mut() {
            if v == gone {
                *v = survivor.clone();
            }
        }
        if let Some(at) = self.persisted_at.get(gone).copied() {
            let e = self.persisted_at.entry(survivor.clone()).or_insert(at);
            *e = (*e).min(at);
        }
        self.tombstones.insert(gone.clone(), (survivor.clone(), now_up + TOMBSTONE_MS));
    }

    /// A process exited: the key stays (history), the process binding goes.
    pub fn release_process(&mut self, uid: &SessionUid) {
        if let Some(p) = self.process_of.remove(uid) {
            if self.by_process.get(&p) == Some(uid) {
                self.by_process.remove(&p);
            }
        }
    }

    /// Tombstones whose 10 min passed (the caller sends `Remove`).
    pub fn expire_tombstones(&mut self, now_up: u64) -> Vec<SessionUid> {
        let gone: Vec<SessionUid> =
            self.tombstones.iter().filter(|(_, (_, exp))| *exp <= now_up).map(|(u, _)| u.clone()).collect();
        for u in &gone {
            self.tombstones.remove(u);
        }
        gone
    }

    /// The earliest tombstone expiry (a wake-up deadline).
    pub fn next_tombstone_expiry(&self) -> Option<u64> {
        self.tombstones.values().map(|(_, e)| *e).min()
    }

    pub fn tombstone_target(&self, uid: &SessionUid) -> Option<&SessionUid> {
        self.tombstones.get(uid).map(|(t, _)| t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> AccountId {
        AccountId::derive(Vendor::Claude, "/Users/u/.claude")
    }

    fn hint(vsid: Option<&str>, account: Option<AccountId>, process: Option<ProcessKey>) -> IdentityHint {
        IdentityHint {
            vendor: Some(Vendor::Claude),
            account_id: account,
            vendor_session_id: vsid.map(str::to_string),
            process,
            gilde_spawn_id: None,
        }
    }

    #[test]
    fn rules_in_order() {
        let mut ix = IdentityIndex::default();
        let p = ProcessKey::new(100, 5);
        // 5: process-only session.
        let Resolved::Uid { uid: u, rule, .. } = ix.resolve(&hint(None, None, Some(p.clone())), None, false) else {
            panic!()
        };
        assert_eq!(rule, 5);
        ix.bind(&u, &hint(None, None, Some(p.clone())), None, 0);
        // 4: the process again, now with a vendor id → attach.
        let h = hint(Some("326555f6-0000-4000-8000-000000000001"), Some(acct()), Some(p.clone()));
        let Resolved::Uid { uid: u2, rule, .. } = ix.resolve(&h, None, false) else { panic!() };
        assert_eq!((rule, &u2), (4, &u));
        let b = ix.bind(&u2, &h, Some(acct()), 0);
        assert!(b.newly_keyed);
        // 2: by key.
        let h2 = hint(Some("326555f6-0000-4000-8000-000000000001"), Some(acct()), None);
        let Resolved::Uid { uid: u3, rule, .. } = ix.resolve(&h2, None, false) else { panic!() };
        assert_eq!((rule, u3), (2, u));
    }

    #[test]
    fn teammate_without_account_holds_then_files_under_unknown() {
        let ix = IdentityIndex::default();
        let h = hint(Some("aaaaaaaa-0000-4000-8000-000000000001"), None, None);
        assert_eq!(ix.resolve(&h, None, false), Resolved::Hold);
        let Resolved::Uid { account, rule, .. } = ix.resolve(&h, None, true) else { panic!() };
        assert_eq!((account, rule), (None, 5));
        let Resolved::Uid { account, .. } = ix.resolve(&h, Some(acct()), false) else { panic!() };
        assert_eq!(account, Some(acct()));
    }

    #[test]
    fn the_older_persisted_uid_survives_a_merge() {
        let mut ix = IdentityIndex::default();
        let key = IdentityKey {
            vendor: Vendor::Claude,
            account_id: Some(acct()),
            vendor_session_id: "bbbbbbbb-0000-4000-8000-000000000001".into(),
        };
        let old = SessionUid::mint();
        ix.load(old.clone(), Some(key.clone()), 1_000);
        // A provisional spawn-only session learns the same key.
        let newer = ix.bind_spawn("sp-1", None);
        let h = IdentityHint {
            vendor: Some(Vendor::Claude),
            account_id: Some(acct()),
            vendor_session_id: Some(key.vendor_session_id.clone()),
            process: None,
            gilde_spawn_id: Some("sp-1".into()),
        };
        let out = ix.bind(&newer, &h, Some(acct()), 50);
        assert_eq!(out.merged, Some((old.clone(), newer.clone())));
        assert_eq!(ix.live(&newer), old);
        assert_eq!(ix.uid_of_spawn("sp-1"), Some(old.clone()));
        assert_eq!(ix.expire_tombstones(50 + TOMBSTONE_MS), vec![newer]);
    }

    #[test]
    fn a_second_key_for_a_keyed_uid_rekeys_the_process() {
        let mut ix = IdentityIndex::default();
        let p = ProcessKey::new(7, 7);
        let h1 = hint(Some("cccccccc-0000-4000-8000-000000000001"), Some(acct()), Some(p.clone()));
        let Resolved::Uid { uid, .. } = ix.resolve(&h1, None, false) else { panic!() };
        ix.bind(&uid, &h1, Some(acct()), 0);
        let h2 = hint(Some("dddddddd-0000-4000-8000-000000000001"), Some(acct()), Some(p.clone()));
        let Resolved::Uid { uid: via, rule, .. } = ix.resolve(&h2, None, false) else { panic!() };
        assert_eq!((rule, &via), (4, &uid));
        let out = ix.bind(&via, &h2, Some(acct()), 0);
        let (old, new) = out.rekeyed.expect("rekeyed");
        assert_eq!(old, uid);
        assert_ne!(new, uid);
        assert_eq!(ix.uid_of_process(&p), Some(new.clone()));
        assert_eq!(ix.key_of(&uid).unwrap().vendor_session_id, "cccccccc-0000-4000-8000-000000000001");
    }
}
