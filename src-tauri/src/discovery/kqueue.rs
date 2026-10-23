//! kqueue observers (contract §5.9): `EVFILT_PROC NOTE_EXIT` for every tracked
//! pid (≤ 1,024; beyond that the scan cadence covers exits), keyed by
//! `(pid, start)` so a reused pid is never confused with the process we
//! tracked; and `EVFILT_VNODE` on a few directories that need a
//! non-recursive "an entry appeared or vanished" signal (`$HOME` for the
//! `~/.claude-*` siblings and the orchestrator dir).
//!
//! Why kqueue and not FSEvents for `$HOME`: an FSEvents stream has no
//! non-recursive mode — every change anywhere under `$HOME` would wake the
//! process and be filtered afterwards, which breaks the idle budget (§11.1).
//! A vnode watch on the directory fd fires only when *its* entries change.
//!
//! Observation only: nothing here signals a process. Directory fds are opened
//! `O_EVTONLY` (event notification only, never read or written).

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// The cap on tracked pids (§5.9).
pub const MAX_TRACKED: usize = 1024;

const WAKE_IDENT: usize = 0x6769_6c64; // "gild"

/// What the kqueue thread reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KqEvent {
    /// The tracked process `(pid, start)` exited.
    Exit { pid: i32, start_sec: i64 },
    /// A watched directory's entries changed.
    Dir { token: u64 },
}

/// The outcome of [`KqWatcher::track`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Tracked,
    AlreadyTracked,
    /// The process is already gone (or the pid now belongs to another
    /// process): an `Exit` was sent.
    AlreadyGone,
    /// Over [`MAX_TRACKED`]: the scan cadence covers it.
    Full,
}

struct Inner {
    kq: OwnedFd,
    tracked: Mutex<HashMap<i32, i64>>,
    dirs: Mutex<HashMap<u64, OwnedFd>>,
    on: Arc<dyn Fn(KqEvent) + Send + Sync>,
}

/// One kqueue and its thread.
pub struct KqWatcher {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

fn kev(ident: usize, filter: i16, flags: u16, fflags: u32, udata: usize) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: udata as *mut libc::c_void,
    }
}

fn apply(kq: i32, ch: &libc::kevent) -> Result<(), i32> {
    // SAFETY: one valid change, no event list.
    let r = unsafe { libc::kevent(kq, ch, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if r < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
    } else {
        Ok(())
    }
}

impl KqWatcher {
    /// Create the kqueue and start its thread; events go to `on`.
    pub fn start(on: Arc<dyn Fn(KqEvent) + Send + Sync>) -> std::io::Result<KqWatcher> {
        // SAFETY: plain syscall.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh fd we own.
        let kq = unsafe { OwnedFd::from_raw_fd(fd) };
        apply(
            kq.as_raw_fd(),
            &kev(WAKE_IDENT, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0, 0),
        )
        .map_err(std::io::Error::from_raw_os_error)?;
        let inner = Arc::new(Inner {
            kq,
            tracked: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            on,
        });
        let thread_inner = inner.clone();
        let thread = std::thread::Builder::new()
            .name("gilde-discovery-kqueue".into())
            .spawn(move || run(thread_inner))?;
        Ok(KqWatcher { inner, thread: Some(thread) })
    }

    fn send(&self, ev: KqEvent) {
        (self.inner.on)(ev);
    }

    /// Track `(pid, start_sec)`. `start_of` re-reads the kernel start time of
    /// `pid` after registering, so a pid reused between the scan and the
    /// registration is caught.
    pub fn track(&self, pid: i32, start_sec: i64, start_of: impl Fn(i32) -> Option<i64>) -> Track {
        if pid <= 0 {
            return Track::AlreadyGone;
        }
        let mut tracked = match self.inner.tracked.lock() {
            Ok(t) => t,
            Err(_) => return Track::Full,
        };
        match tracked.get(&pid) {
            Some(s) if *s == start_sec => return Track::AlreadyTracked,
            Some(old) => {
                // The pid was reused: the process we tracked is gone.
                let old = *old;
                tracked.remove(&pid);
                drop(tracked);
                self.send(KqEvent::Exit { pid, start_sec: old });
                tracked = match self.inner.tracked.lock() {
                    Ok(t) => t,
                    Err(_) => return Track::Full,
                };
            }
            None => {}
        }
        if tracked.len() >= MAX_TRACKED {
            return Track::Full;
        }
        let ch = kev(
            pid as usize,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_ONESHOT,
            libc::NOTE_EXIT,
            0,
        );
        match apply(self.inner.kq.as_raw_fd(), &ch) {
            Ok(()) => {}
            Err(libc::ESRCH) => {
                drop(tracked);
                self.send(KqEvent::Exit { pid, start_sec });
                return Track::AlreadyGone;
            }
            Err(_) => return Track::Full,
        }
        if start_of(pid) != Some(start_sec) {
            let del = kev(pid as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0, 0);
            let _ = apply(self.inner.kq.as_raw_fd(), &del);
            drop(tracked);
            self.send(KqEvent::Exit { pid, start_sec });
            return Track::AlreadyGone;
        }
        tracked.insert(pid, start_sec);
        Track::Tracked
    }

    /// Stop tracking `pid` (no event).
    pub fn untrack(&self, pid: i32) {
        if let Ok(mut t) = self.inner.tracked.lock() {
            if t.remove(&pid).is_some() {
                let del = kev(pid as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0, 0);
                let _ = apply(self.inner.kq.as_raw_fd(), &del);
            }
        }
    }

    pub fn tracked_count(&self) -> usize {
        self.inner.tracked.lock().map(|t| t.len()).unwrap_or(0)
    }

    /// Watch `dir`'s entries; `token` comes back in [`KqEvent::Dir`].
    pub fn watch_dir(&self, dir: &Path, token: u64) -> std::io::Result<()> {
        let c = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: valid C string; O_EVTONLY opens for event notification only.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let ch = kev(
            fd.as_raw_fd() as usize,
            libc::EVFILT_VNODE,
            libc::EV_ADD | libc::EV_CLEAR,
            libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_LINK,
            token as usize,
        );
        apply(self.inner.kq.as_raw_fd(), &ch).map_err(std::io::Error::from_raw_os_error)?;
        if let Ok(mut d) = self.inner.dirs.lock() {
            d.insert(token, fd); // replacing an old fd closes it, which removes its knote
        }
        Ok(())
    }

    pub fn unwatch_dir(&self, token: u64) {
        if let Ok(mut d) = self.inner.dirs.lock() {
            d.remove(&token);
        }
    }

    pub fn is_watching_dir(&self, token: u64) -> bool {
        self.inner.dirs.lock().map(|d| d.contains_key(&token)).unwrap_or(false)
    }
}

impl Drop for KqWatcher {
    fn drop(&mut self) {
        let wake = kev(WAKE_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0);
        let _ = apply(self.inner.kq.as_raw_fd(), &wake);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(inner: Arc<Inner>) {
    super::set_thread_qos(false);
    let mut events: [libc::kevent; 16] =
        // SAFETY: all-zero kevents are valid output slots.
        unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `events` is valid for 16 entries; NULL timeout blocks.
        let n = unsafe {
            libc::kevent(
                inner.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                std::ptr::null(),
            )
        };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        for ev in &events[..n as usize] {
            match ev.filter {
                libc::EVFILT_USER if ev.ident == WAKE_IDENT => return,
                libc::EVFILT_PROC if ev.fflags & libc::NOTE_EXIT != 0 => {
                    let pid = ev.ident as i32;
                    let start = inner.tracked.lock().ok().and_then(|mut t| t.remove(&pid));
                    if let Some(start_sec) = start {
                        (inner.on)(KqEvent::Exit { pid, start_sec });
                    }
                }
                libc::EVFILT_VNODE => (inner.on)(KqEvent::Dir { token: ev.udata as u64 }),
                _ => {}
            }
        }
    }
}
