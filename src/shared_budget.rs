//! Optional daemon-free node credits. Linux open-file-description range locks
//! release on process exit, including SIGKILL. All participants use one private
//! file and identical limits. flock serializes the all-or-none admission step.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};
const MEMORY: i64 = 4096;
const BACKLOG: i64 = 1 << 42;
const FILES: i64 = 1 << 44;
const SYNC: i64 = 1 << 46;
pub struct SyncPermit {
    _file: File,
    begin_ns: u128,
    end_ns: u128,
}
impl SyncPermit {
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"begin_ns":self.begin_ns,"admitted_ns":self.end_ns,"wait_ns":self.end_ns-self.begin_ns,"node_core_sync_limit":1,"wait":"kernel OFD lock; durability attempted after target detach"})
    }
}
pub fn automatic_path() -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let uid = unsafe { libc::geteuid() };
    let mut candidates = Vec::new();
    if let Ok(custom) = std::env::var("PODDUMP_RUNTIME_DIR") {
        if !custom.is_empty() {
            candidates.push(PathBuf::from(custom));
        }
    }
    for base in ["/run", "/var/run", "/var/tmp"] {
        candidates.push(PathBuf::from(format!("{base}/poddump-{uid}")));
    }
    let mut last_err = String::from("no writable runtime directory candidate");
    for directory in candidates {
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => {
                last_err = format!("create shared resource directory {}: {e}", directory.display());
                continue;
            }
        }
        let metadata = match std::fs::symlink_metadata(&directory) {
            Ok(m) => m,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        if !metadata.file_type().is_dir()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
        {
            last_err =
                "automatic resource directory must be private and owned by the caller".into();
            continue;
        }
        return Ok(directory.join("credits-v3"));
    }
    Err(last_err)
}
pub struct SharedBudget {
    path: PathBuf,
    anchor: File,
    jobs: usize,
    memory: usize,
    backlog: usize,
    pending: usize,
}
pub struct SharedLease {
    file: File,
    locks: Vec<(i64, i64)>,
    memory: Option<(i64, i64)>,
    capture: Option<(i64, i64)>,
    wait_ns: u128,
    attempts: u64,
    begin_ns: u128,
    end_ns: u128,
    probes: ProbeStats,
    ticket: u64,
}
#[derive(Default)]
struct ProbeStats {
    open_ns: u128,
    queries: u64,
    flock_busy: u64,
    sleep_requested_ns: u64,
    blocked: [u64; 4],
    blocked_queue: u64,
    queue_ahead_max: usize,
}
struct Reservation {
    locks: Vec<(i64, i64)>,
    memory: Option<(i64, i64)>,
    capture: (i64, i64),
    admitted_ns: u128,
}
fn lock(file: &File, start: i64, len: i64, kind: i16) -> Result<(), String> {
    let mut f: libc::flock = unsafe { std::mem::zeroed() };
    f.l_type = kind;
    f.l_whence = libc::SEEK_SET as i16;
    f.l_start = start;
    f.l_len = len;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &f) } == -1 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}
fn gap(
    file: &File,
    base: i64,
    capacity: usize,
    size: usize,
    probes: &mut ProbeStats,
) -> Result<Option<(i64, i64)>, String> {
    if size == 0 {
        return Ok(Some((base, 0)));
    }
    let mut at = base;
    let end = base + capacity as i64;
    while at + size as i64 <= end {
        let mut f: libc::flock = unsafe { std::mem::zeroed() };
        f.l_type = libc::F_WRLCK as i16;
        f.l_whence = libc::SEEK_SET as i16;
        f.l_start = at;
        f.l_len = size as i64;
        probes.queries += 1;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut f) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if f.l_type == libc::F_UNLCK as i16 {
            return Ok(Some((at, size as i64)));
        }
        if f.l_len == 0 {
            return Ok(None);
        }
        at = f.l_start + f.l_len;
    }
    Ok(None)
}
fn open(path: &Path) -> Result<File, String> {
    open_with(path, true)
}
fn open_with(path: &Path, create: bool) -> Result<File, String> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| e.to_string())?;
    let m = f.metadata().map_err(|e| e.to_string())?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
    {
        return Err(
            "shared budget must be a private regular file owned by the caller, without hard links"
                .into(),
        );
    }
    Ok(f)
}
fn try_flock(file: &File) -> Result<bool, String> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if matches!(
        e.raw_os_error(),
        Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
    ) {
        Ok(false)
    } else {
        Err(e.to_string())
    }
}
impl SharedBudget {
    fn check_namespace(&self, file: &File) -> Result<(), String> {
        let anchor = self.anchor.metadata().map_err(|e| e.to_string())?;
        let current = file.metadata().map_err(|e| e.to_string())?;
        let named = std::fs::symlink_metadata(&self.path).map_err(|e| e.to_string())?;
        if !named.is_file()
            || anchor.dev() != current.dev()
            || anchor.ino() != current.ino()
            || anchor.dev() != named.dev()
            || anchor.ino() != named.ino()
            || current.nlink() != 1
            || current.uid() != unsafe { libc::geteuid() }
            || current.mode() & 0o077 != 0
        {
            return Err(
                "shared credit file was removed or replaced while this collector was active".into(),
            );
        }
        Ok(())
    }
    fn open_instance(&self) -> Result<File, String> {
        // Independent OFD for lock conflict semantics, but never a new inode.
        let file = open_with(&self.path, false)?;
        self.check_namespace(&file)?;
        Ok(file)
    }

    /// No target is stopped here. A separate OFD owns the sync credit so close,
    /// unwind and process exit release it without a separate unlock operation.
    /// Like fsync itself, final durability may outlive capture cancellation.
    pub fn acquire_sync(&self) -> Result<SyncPermit, String> {
        let begin_ns = crate::now();
        let file = self.open_instance()?;
        let mut range: libc::flock = unsafe { std::mem::zeroed() };
        range.l_type = libc::F_WRLCK as i16;
        range.l_whence = libc::SEEK_SET as i16;
        range.l_start = SYNC;
        range.l_len = 1;
        loop {
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLKW, &range) } == 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.to_string());
            }
        }
        self.check_namespace(&file)?;
        Ok(SyncPermit {
            _file: file,
            begin_ns,
            end_ns: crate::now(),
        })
    }
    #[cfg(test)]
    pub fn new(path: &Path, jobs: usize, memory: usize, backlog: usize) -> Result<Self, String> {
        Self::new_with_token(
            path,
            jobs,
            memory,
            backlog,
            &crate::engine::CancellationToken::with_timeout(60000),
        )
    }
    #[cfg(test)]
    pub fn new_with_token(
        path: &Path,
        jobs: usize,
        memory: usize,
        backlog: usize,
        token: &crate::engine::CancellationToken,
    ) -> Result<Self, String> {
        Self::new_with_limits(path, jobs, memory, backlog, 64, token)
    }
    pub fn new_with_limits(
        path: &Path,
        jobs: usize,
        memory: usize,
        backlog: usize,
        pending: usize,
        token: &crate::engine::CancellationToken,
    ) -> Result<Self, String> {
        if !(1..=64).contains(&pending) {
            return Err("shared pending limit must be1..64".into());
        }
        if memory >= (1usize << 40) || backlog >= (1usize << 40) {
            return Err("shared limits must be below1TiB".into());
        }
        let mut f = open(path)?;
        while !try_flock(&f)? {
            if token.is_cancelled() {
                return Err("shared initialization cancelled/deadline exceeded".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if token.is_cancelled() {
            return Err("shared initialization cancelled/deadline exceeded".into());
        }
        // v3 adds registered FIFO admission with OFD waiter liveness.
        // Public callers
        // use a separate automatic file so stale experimental v1 configuration
        // is never rewritten while another collector might still hold it.
        let config = format!(
            "poddump-shared-v3 jobs={jobs} memory={memory} backlog={backlog} pending={pending} sync_slots=1 queue_slots=256 fifo=registered\n"
        );
        let mut current = String::new();
        (&mut f)
            .take(config.len() as u64)
            .read_to_string(&mut current)
            .map_err(|e| e.to_string())?;
        if current.is_empty() {
            f.write_all(config.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        } else if current != config {
            return Err(
                "shared budget configuration mismatch; use the same limits for every caller".into(),
            );
        }
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let budget = Self {
            path: path.into(),
            anchor: f,
            jobs,
            memory,
            backlog,
            pending,
        };
        budget.check_namespace(&budget.anchor)?;
        Ok(budget)
    }
    pub fn acquire(
        &self,
        memory: usize,
        backlog: usize,
        token: &crate::engine::CancellationToken,
    ) -> Result<SharedLease, String> {
        if memory > self.memory {
            return Err("snapshot reservation exceeds shared capacity before stopping".into());
        }
        let begin_ns = crate::now();
        let mut attempts = 0;
        // Each request still owns an independent OFD. Reopening this same file
        // on every poll adds path lookup/stat/close work across waiting callers.
        // Never share one OFD between requests: its locks would not conflict.
        let file = self.open_instance()?;
        let mut probes = ProbeStats {
            open_ns: crate::now() - begin_ns,
            ..Default::default()
        };
        let ticket = loop {
            if token.is_cancelled() {
                return Err(
                    "shared queue registration cancelled/deadline exceeded before stopping".into(),
                );
            }
            if let Some(t) = self.try_register(&file, &mut probes)? {
                break t;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        loop {
            if token.is_cancelled() {
                return Err("shared admission cancelled/deadline exceeded before stopping".into());
            }
            attempts += 1;
            if let Some(reserved) = self.try_reserve(&file, memory, backlog, &mut probes, ticket)? {
                return Ok(SharedLease {
                    file,
                    locks: reserved.locks,
                    memory: reserved.memory,
                    capture: Some(reserved.capture),
                    wait_ns: reserved.admitted_ns.saturating_sub(begin_ns),
                    attempts,
                    begin_ns,
                    end_ns: reserved.admitted_ns,
                    probes,
                    ticket: ticket.id,
                });
            }
            // Brief waits stay responsive. Long contention backs off while
            // targets are still running; deterministic per-OFD jitter avoids
            // waking every independent caller on the same 2ms boundary.
            let base = if attempts <= 4 {
                2
            } else if attempts <= 12 {
                4
            } else {
                8
            };
            let jitter = if base == 2 {
                0
            } else {
                (attempts + file.as_raw_fd() as u64 + std::process::id() as u64) % 3
            };
            let delay = base + jitter;
            probes.sleep_requested_ns += delay * 1_000_000;
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    #[cfg(test)]
    fn try_acquire(&self, memory: usize, backlog: usize) -> Result<Option<SharedLease>, String> {
        if memory > self.memory {
            return Err("snapshot reservation exceeds shared capacity before stopping".into());
        }
        let f = self.open_instance()?;
        let mut probes = ProbeStats::default();
        let Some(ticket) = self.try_register(&f, &mut probes)? else {
            return Ok(None);
        };
        Ok(self
            .try_reserve(&f, memory, backlog, &mut probes, ticket)?
            .map(|r| SharedLease {
                file: f,
                locks: r.locks,
                memory: r.memory,
                capture: Some(r.capture),
                wait_ns: 0,
                attempts: 1,
                begin_ns: 0,
                end_ns: 0,
                probes,
                ticket: ticket.id,
            }))
    }
    fn try_register(
        &self,
        f: &File,
        probes: &mut ProbeStats,
    ) -> Result<Option<crate::shared_queue::Ticket>, String> {
        if !try_flock(f)? {
            probes.flock_busy += 1;
            return Ok(None);
        }
        let result = (|| {
            self.check_namespace(f)?;
            crate::shared_queue::register(f)
        })();
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        result
    }
    fn try_reserve(
        &self,
        f: &File,
        memory: usize,
        backlog: usize,
        probes: &mut ProbeStats,
        ticket: crate::shared_queue::Ticket,
    ) -> Result<Option<Reservation>, String> {
        if !try_flock(&f)? {
            probes.flock_busy += 1;
            return Ok(None);
        }
        let result = (|| {
            self.check_namespace(f)?;
            let ahead = crate::shared_queue::ahead(f, ticket)?;
            probes.queue_ahead_max = probes.queue_ahead_max.max(ahead);
            if ahead > 0 {
                probes.blocked_queue += 1;
                return Ok(None);
            }
            // A missing resource makes admission impossible. Do not scan the
            // remaining lock ranges or reserve a subset while waiting.
            let mut slots = [(0, 0); 4];
            for (i, (base, capacity, size)) in [
                (1, self.jobs, 1),
                (MEMORY, self.memory, memory),
                (BACKLOG, self.backlog, backlog.min(self.backlog)),
                (FILES, self.pending, 1),
            ]
            .into_iter()
            .enumerate()
            {
                match gap(f, base, capacity, size, probes)? {
                    Some(slot) => slots[i] = slot,
                    None => {
                        probes.blocked[i] += 1;
                        return Ok(None);
                    }
                }
            }
            let (c, m, b, n) = (slots[0], slots[1], slots[2], slots[3]);
            let locks = vec![c, m, b, n]
                .into_iter()
                .filter(|x| x.1 > 0)
                .collect::<Vec<_>>();
            for (at, len) in &locks {
                lock(&f, *at, *len, libc::F_WRLCK as i16)?;
            }
            let admitted_ns = crate::now();
            crate::shared_queue::depart(f, ticket)?;
            Ok(Some(Reservation {
                locks,
                memory: (m.1 > 0).then_some(m),
                capture: c,
                admitted_ns,
            }))
        })();
        // The waiting FD remains open, so every unsuccessful probe must unlock
        // flock explicitly. On any error the caller drops the FD and its OFD locks.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        result
    }
}
impl SharedLease {
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"begin_ns":self.begin_ns,"end_ns":self.end_ns,"wait_ns":self.wait_ns,"attempts":self.attempts,"open_calls":1,"open_ns":self.probes.open_ns,"lock_queries":self.probes.queries,"flock_busy":self.probes.flock_busy,"sleep_requested_ns":self.probes.sleep_requested_ns,"blocked_capture_memory_backlog_files":self.probes.blocked,"admission_ticket":self.ticket,"queued_ahead_max":self.probes.queue_ahead_max,"blocked_queue":self.probes.blocked_queue,"queue_slots":crate::shared_queue::SLOTS,"policy":"registered FIFO; all-or-none capture, snapshot, backlog and descriptor OFD credits; OFD liveness reaps cancelled/exited waiters; credit admission before target attach; streaming I/O can delay stopped targets; bounded admission deadline"})
    }

    fn release(&mut self, range: Option<(i64, i64)>) -> Result<(), String> {
        if let Some((start, len)) = range {
            lock(&self.file, start, len, libc::F_UNLCK as i16)?;
            self.locks.retain(|x| *x != (start, len));
        }
        Ok(())
    }
    pub fn release_capture(&mut self) -> Result<(), String> {
        let r = self.capture.take();
        self.release(r)
    }
    pub fn release_memory(&mut self) -> Result<(), String> {
        let r = self.memory.take();
        self.release(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn queued(budget: &SharedBudget) -> usize {
        let f = budget.open_instance().unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(2);
        while !try_flock(&f).unwrap() {
            assert!(std::time::Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        }
        let n = crate::shared_queue::live(&f).unwrap();
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) }, 0);
        n
    }
    fn await_queued(budget: &SharedBudget, n: usize) {
        let end = std::time::Instant::now() + Duration::from_secs(3);
        while queued(budget) != n {
            assert!(
                std::time::Instant::now() < end,
                "queue did not reach expected size"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn queue_budget() -> (PathBuf, std::sync::Arc<SharedBudget>) {
        let path = std::env::temp_dir().join(format!(
            "poddump_fifo_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let b = SharedBudget::new_with_limits(
            &path,
            3,
            16,
            128,
            4,
            &crate::engine::CancellationToken::with_timeout(5000),
        )
        .unwrap();
        (path, std::sync::Arc::new(b))
    }
    #[test]
    fn fifo_blocks_a_fitting_younger_until_older_reservation_finishes() {
        let (path, b) = queue_budget();
        let held = b.try_acquire(8, 1).unwrap().unwrap();
        let (oa, or) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let older = std::sync::Arc::clone(&b);
        let a = std::thread::spawn(move || {
            let lease = older
                .acquire(16, 1, &crate::engine::CancellationToken::with_timeout(5000))
                .unwrap();
            oa.send(lease.diagnostics()["admission_ticket"].as_u64().unwrap())
                .unwrap();
            wait.recv().unwrap();
            drop(lease);
        });
        await_queued(&b, 1);
        let (ya, yr) = std::sync::mpsc::channel();
        let younger = std::sync::Arc::clone(&b);
        let c = std::thread::spawn(move || {
            let lease = younger
                .acquire(1, 1, &crate::engine::CancellationToken::with_timeout(5000))
                .unwrap();
            ya.send(lease.diagnostics()["admission_ticket"].as_u64().unwrap())
                .unwrap();
        });
        await_queued(&b, 2);
        assert!(yr.recv_timeout(Duration::from_millis(40)).is_err());
        drop(held);
        let first = or.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(yr.recv_timeout(Duration::from_millis(40)).is_err());
        release.send(()).unwrap();
        let second = yr.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(first < second);
        a.join().unwrap();
        c.join().unwrap();
        assert_eq!(queued(&b), 0);
        drop(b);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn cancellation_of_queued_head_allows_younger_while_prior_holder_remains() {
        let (path, b) = queue_budget();
        let held = b.try_acquire(8, 1).unwrap().unwrap();
        let token = crate::engine::CancellationToken::with_timeout(5000);
        let t = token.clone();
        let older = std::sync::Arc::clone(&b);
        let a = std::thread::spawn(move || older.acquire(16, 1, &t).map(|_| ()).unwrap_err());
        await_queued(&b, 1);
        let (ready, done) = std::sync::mpsc::channel();
        let younger = std::sync::Arc::clone(&b);
        let c = std::thread::spawn(move || {
            let _lease = younger
                .acquire(1, 1, &crate::engine::CancellationToken::with_timeout(5000))
                .unwrap();
            ready.send(()).unwrap();
        });
        await_queued(&b, 2);
        assert!(done.recv_timeout(Duration::from_millis(40)).is_err());
        token.cancel();
        assert!(a.join().unwrap().contains("cancelled"));
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        c.join().unwrap();
        drop(held);
        assert_eq!(queued(&b), 0);
        drop(b);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn fifo_killed_head_helper() {
        let Ok(path) = std::env::var("PODDUMP_FIFO_KILLED_HEAD") else {
            return;
        };
        let b = SharedBudget::new_with_limits(
            Path::new(&path),
            3,
            16,
            128,
            4,
            &crate::engine::CancellationToken::with_timeout(15000),
        )
        .unwrap();
        let _lease = b
            .acquire(
                16,
                1,
                &crate::engine::CancellationToken::with_timeout(15000),
            )
            .unwrap();
        panic!("helper should be killed while queued");
    }
    #[test]
    fn killed_queued_process_does_not_block_younger_or_leak_resources() {
        let (path, b) = queue_budget();
        let held = b.try_acquire(8, 1).unwrap().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_budget::tests::fifo_killed_head_helper",
                "--test-threads=1",
            ])
            .env("PODDUMP_FIFO_KILLED_HEAD", &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        await_queued(&b, 1);
        let (ready, done) = std::sync::mpsc::channel();
        let younger = std::sync::Arc::clone(&b);
        let thread = std::thread::spawn(move || {
            let _lease = younger
                .acquire(1, 1, &crate::engine::CancellationToken::with_timeout(5000))
                .unwrap();
            ready.send(()).unwrap();
        });
        await_queued(&b, 2);
        assert!(done.recv_timeout(Duration::from_millis(40)).is_err());
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
        drop(held);
        assert_eq!(queued(&b), 0);
        drop(b);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn impossible_reservation_is_rejected_before_queue_and_keeps_capacity_available() {
        let (path, b) = queue_budget();
        assert!(
            b.acquire(17, 1, &crate::engine::CancellationToken::with_timeout(5000))
                .is_err()
        );
        assert_eq!(queued(&b), 0);
        drop(b.try_acquire(16, 128).unwrap().unwrap());
        drop(b);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn node_sync_wait_does_not_hold_admission_flock_and_unwind_releases_slot() {
        let path = std::env::temp_dir().join(format!(
            "poddump_sync_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let a = SharedBudget::new(&path, 4, 512, 1024).unwrap();
        let b = SharedBudget::new(&path, 4, 512, 1024).unwrap();
        let held = a.acquire_sync().unwrap();
        let (ready, started) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            ready.send(()).unwrap();
            let _permit = b.acquire_sync().unwrap();
            done.send(()).unwrap();
        });
        started.recv().unwrap();
        assert!(finished.recv_timeout(Duration::from_millis(30)).is_err());
        let capture = a.try_acquire(512, 1024).unwrap().unwrap();
        drop(capture);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = held;
            panic!("synthetic finalizer unwind");
        }));
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        thread.join().unwrap();
        drop(a.acquire_sync().unwrap());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn global_pending_cap_survives_capture_and_memory_release_until_durability() {
        let path = std::env::temp_dir().join(format!(
            "poddump_pending_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let token = crate::engine::CancellationToken::with_timeout(1000);
        let a = SharedBudget::new_with_limits(&path, 4, 512, 1024, 1, &token).unwrap();
        let b = SharedBudget::new_with_limits(&path, 4, 512, 1024, 1, &token).unwrap();
        assert!(SharedBudget::new(&path, 4, 512, 1024).is_err());
        let mut held = a.try_acquire(128, 128).unwrap().unwrap();
        held.release_capture().unwrap();
        held.release_memory().unwrap();
        assert!(b.try_acquire(128, 128).unwrap().is_none());
        drop(held);
        assert!(b.try_acquire(128, 128).unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn blocked_probe_releases_flock_and_reserves_nothing() {
        let path = std::env::temp_dir().join(format!(
            "poddump_retry_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let budget = SharedBudget::new(&path, 1, 512, 1024).unwrap();
        let held = budget.try_acquire(128, 128).unwrap().unwrap();
        let waiting = open(&path).unwrap();
        let mut stats = ProbeStats::default();
        let ticket = budget.try_register(&waiting, &mut stats).unwrap().unwrap();
        assert!(
            budget
                .try_reserve(&waiting, 128, 128, &mut stats, ticket)
                .unwrap()
                .is_none()
        );
        assert_eq!(stats.blocked, [1, 0, 0, 0]);
        let other = open(&path).unwrap();
        assert!(try_flock(&other).unwrap(), "waiting request retained flock");
        drop(other);
        drop(held);
        assert!(
            budget
                .try_reserve(&waiting, 512, 1024, &mut stats, ticket)
                .unwrap()
                .is_some()
        );
        drop(waiting);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn waiting_request_keeps_one_independent_ofd_until_admission() {
        let path = std::env::temp_dir().join(format!(
            "poddump_wait_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let budget = SharedBudget::new(&path, 1, 512, 1024).unwrap();
        let held = budget.try_acquire(512, 1024).unwrap().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(held);
        });
        let lease = budget
            .acquire(
                512,
                1024,
                &crate::engine::CancellationToken::with_timeout(1000),
            )
            .unwrap();
        assert_eq!(lease.diagnostics()["open_calls"], 1);
        assert!(lease.attempts > 1);
        assert!(lease.probes.blocked[0] > 0);
        drop(lease);
        releaser.join().unwrap();
        assert!(budget.try_acquire(512, 1024).unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn child_lock_holder() {
        if let Ok(path) = std::env::var("PODDUMP_LOCK_TEST_FILE") {
            let b = SharedBudget::new(Path::new(&path), 2, 512, 1024).unwrap();
            let _lease = b.try_acquire(512, 1024).unwrap().unwrap();
            let _sync = b.acquire_sync().unwrap();
            println!("credits-held");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(10));
        }
    }
    #[test]
    fn sigkill_returns_other_process_credits() {
        use std::io::BufRead;
        let path = std::env::temp_dir().join(format!(
            "poddump_crash_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let b = SharedBudget::new(&path, 2, 512, 1024).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_budget::tests::child_lock_holder",
                "--nocapture",
            ])
            .env("PODDUMP_LOCK_TEST_FILE", &path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(input.read_line(&mut line).unwrap() > 0);
            if line.contains("credits-held") {
                break;
            }
        }
        assert!(b.try_acquire(1, 1).unwrap().is_none());
        assert!(lock(&open(&path).unwrap(), SYNC, 1, libc::F_WRLCK as i16).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(b.try_acquire(512, 1024).unwrap().is_some());
        drop(b.acquire_sync().unwrap());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn independent_instances_share_and_return_crash_safe_credits() {
        let path = std::env::temp_dir().join(format!(
            "poddump_shared_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let a = SharedBudget::new(&path, 2, 512, 1024).unwrap();
        let b = SharedBudget::new(&path, 2, 512, 1024).unwrap();
        let mut one = a.try_acquire(300, 400).unwrap().unwrap();
        assert!(b.try_acquire(300, 400).unwrap().is_none());
        one.release_capture().unwrap();
        assert!(b.try_acquire(300, 400).unwrap().is_none());
        one.release_memory().unwrap();
        let two = b.try_acquire(300, 400).unwrap().unwrap();
        assert!(SharedBudget::new(&path, 4, 512, 1024).is_err());
        drop(one);
        drop(two);
        assert!(a.try_acquire(512, 1024).unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod deadline_regressions {
    use super::*;
    #[test]
    fn held_flock_does_not_hide_timeout_or_cancellation() {
        let path = std::env::temp_dir().join(format!(
            "poddump_flock_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let b = SharedBudget::new(&path, 2, 512, 1024).unwrap();
        let held = open(&path).unwrap();
        assert!(try_flock(&held).unwrap());
        let token = crate::engine::CancellationToken::with_timeout(20);
        let begin = std::time::Instant::now();
        assert!(b.acquire(1, 1, &token).is_err());
        assert!(begin.elapsed() < Duration::from_secs(1));
        let token = crate::engine::CancellationToken::with_timeout(20);
        let begin = std::time::Instant::now();
        assert!(SharedBudget::new_with_token(&path, 2, 512, 1024, &token).is_err());
        assert!(begin.elapsed() < Duration::from_secs(1));
        let token = crate::engine::CancellationToken::with_timeout(60000);
        token.cancel();
        assert!(b.acquire(1, 1, &token).is_err());
        drop(held);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod namespace_regressions {
    use super::*;
    fn directory() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "poddump_credit_inode_{}_{}",
            std::process::id(),
            crate::now()
        ));
        std::fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn missing_credit_namespace_is_not_silently_recreated() {
        let dir = directory();
        let path = dir.join("credits");
        let b = SharedBudget::new(&path, 1, 4096, 4096).unwrap();
        let held = b.try_acquire(1, 1).unwrap().unwrap();
        std::fs::rename(&path, dir.join("saved")).unwrap();
        assert!(
            b.acquire(1, 1, &crate::engine::CancellationToken::with_timeout(50))
                .is_err()
        );
        assert!(b.acquire_sync().is_err());
        assert!(!path.exists());
        drop(held);
        drop(b);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn replaced_credit_namespace_is_rejected_with_old_lease_alive() {
        let dir = directory();
        let path = dir.join("credits");
        let b = SharedBudget::new(&path, 1, 4096, 4096).unwrap();
        let held = b.try_acquire(1, 1).unwrap().unwrap();
        std::fs::rename(&path, dir.join("saved")).unwrap();
        let mut replacement = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        replacement.write_all(b"unrelated replacement").unwrap();
        assert!(b.try_acquire(1, 1).is_err());
        assert!(b.acquire_sync().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"unrelated replacement");
        drop(held);
        drop(b);
        drop(replacement);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
