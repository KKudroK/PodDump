#![recursion_limit = "256"]
mod cli_output;
mod corex;
mod identity;
mod pipeline;
mod safe_output;
mod shared_budget;
mod shared_queue;
use serde_json::{Value, json};
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
pub(crate) static TRACE: AtomicBool = AtomicBool::new(true);
static CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn signal(_: i32) {
    CANCEL.store(true, Ordering::Relaxed);
}
pub mod engine {
    #[derive(Default, Clone)]
    pub struct CancellationToken {
        deadline: Option<std::time::Instant>,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl CancellationToken {
        pub fn with_timeout(ms: u64) -> Self {
            Self {
                deadline: Some(std::time::Instant::now() + std::time::Duration::from_millis(ms)),
                cancelled: Default::default(),
            }
        }
        /// A stop budget can shorten the request deadline, never restart it.
        pub fn capped(&self, ms: u64) -> Self {
            let stop = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            Self {
                deadline: Some(self.deadline.map_or(stop, |d| d.min(stop))),
                cancelled: self.cancelled.clone(),
            }
        }
        pub fn timed_out(&self) -> bool {
            self.deadline
                .is_some_and(|d| std::time::Instant::now() >= d)
        }
        pub fn is_cancelled(&self) -> bool {
            super::CANCEL.load(super::Ordering::Relaxed)
                || self.cancelled.load(super::Ordering::Relaxed)
                || self.timed_out()
        }
        pub fn cancel(&self) {
            self.cancelled.store(true, super::Ordering::Relaxed);
        }
    }
}
fn usage() -> Value {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut u) } != 0 {
        return Value::Null;
    }
    json!({"user_us":u.ru_utime.tv_sec*1_000_000+u.ru_utime.tv_usec,"system_us":u.ru_stime.tv_sec*1_000_000+u.ru_stime.tv_usec,"minor_faults":u.ru_minflt,"major_faults":u.ru_majflt,"voluntary_cs":u.ru_nvcsw,"involuntary_cs":u.ru_nivcsw})
}
fn clock_pair() -> Value {
    let before = now();
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let after = now();
    json!({"monotonic_before_ns":before,"wall_unix_utc_ns":utc,"monotonic_after_ns":after,"uncertainty_ns":after-before})
}
fn now() -> u128 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t);
    }
    t.tv_sec as u128 * 1_000_000_000 + t.tv_nsec as u128
}
fn atomic_json(path: &Path, v: &Value, parent: &File) -> Result<(), String> {
    let tmp = path.with_extension("json.partial");
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(serde_json::to_string_pretty(v).unwrap().as_bytes())
        .map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| format!("fsync manifest: {e}"))?;
    safe_output::publish_file(&tmp, path, &f)
        .map_err(|e| format!("publish manifest without overwrite: {e}"))?;
    let output_directory = path.parent().unwrap();
    File::open(output_directory)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("fsync output directory: {e}"))?;
    parent
        .sync_all()
        .map_err(|e| format!("fsync output parent directory: {e}"))
}
fn cleanup_reserve(path: &Path, owned: &File, manifest: &mut Value) -> bool {
    match safe_output::remove_owned(path, owned) {
        Ok(()) => true,
        Err(error) => {
            manifest["status"] = json!("partial");
            manifest["cleanup_errors"] = json!([{"stage":"reserve_cleanup","path":path,"error":error.to_string(),"time_ns":now()}]);
            false
        }
    }
}
fn finalize_core(out: &Path, row: &mut Value, owned_file: Option<File>) {
    if row["status"] != "captured_pending_durability" {
        return;
    }
    row["details"]["durability_begin_ns"] = json!(now());
    let partial = out.join(
        row["partial_file"]
            .as_str()
            .expect("generated partial name"),
    );
    let final_path = partial.with_extension("");
    let result = (|| -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let file = owned_file.ok_or("missing original write file description")?;
        let original = file
            .metadata()
            .map_err(|e| format!("stat original core: {e}"))?;
        let named =
            fs::symlink_metadata(&partial).map_err(|e| format!("stat partial core path: {e}"))?;
        if !named.file_type().is_file()
            || original.dev() != named.dev()
            || original.ino() != named.ino()
        {
            return Err("partial core path no longer names the original captured inode".into());
        }
        row["details"]["inode_verified_ns"] = json!(now());
        row["details"]["sync_thread_usage_begin"] = usage();
        row["details"]["durability_fd"] = json!("original_write_description");
        if row["details"]["durable_ns"].is_null() {
            file.sync_all().map_err(|e| format!("fsync core: {e}"))?;
            row["details"]["durable_ns"] = json!(now());
        }
        if row["details"]["coverage"]["complete"].as_bool() != Some(false) {
            row["details"]["rename_begin_ns"] = json!(now());
            safe_output::publish_file(&partial, &final_path, &file)
                .map_err(|e| format!("publish durable core without overwrite: {e}"))?;
            row["details"]["rename_complete_ns"] = json!(now());
        } else {
            row["details"]["partial_retained_ns"] = json!(now());
        }
        row["details"]["sync_thread_usage_end"] = usage();
        Ok(())
    })();
    row["end_ns"] = json!(now());
    match result {
        Ok(()) => {
            if row["details"]["coverage"]["complete"].as_bool() == Some(false) {
                row["status"] = json!("partial");
                row["error"] = json!("mapped bytes are missing; consult coverage ledger");
                return;
            }
            row["status"] = json!("complete");
            row["file"] = json!(final_path.file_name().unwrap().to_string_lossy());
            row.as_object_mut().unwrap().remove("partial_file");
        }
        Err(error) => {
            row["status"] = json!("failed");
            row["failure_stage"] = json!("durability");
            row["error"] = json!(error);
        }
    }
}
// Receiving and taking an immediately ready batch share one lock lifetime.
// No worker can hold a credit while reacquiring a lock held by an empty recv.
fn receive_ready<T>(queue: &Mutex<std::sync::mpsc::Receiver<T>>, maximum: usize) -> Option<Vec<T>> {
    let rx = queue.lock().unwrap_or_else(|e| e.into_inner());
    let first = rx.recv().ok()?;
    let mut group = vec![first];
    while group.len() < maximum {
        match rx.try_recv() {
            Ok(work) => group.push(work),
            Err(_) => break,
        }
    }
    Some(group)
}
struct WriteWork {
    row: Value,
    captured: corex::Captured,
    lease: pipeline::Lease,
    shared: Option<shared_budget::SharedLease>,
    token: engine::CancellationToken,
}
struct SyncWork {
    row: Value,
    file: File,
    _lease: pipeline::Lease,
    _shared: Option<shared_budget::SharedLease>,
}
fn run() -> Result<bool, String> {
    let clock_start = clock_pair();
    let total_begin = now();
    let total_timer = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 || args.iter().any(|s| s == "--help") {
        #[cfg(not(feature = "development-options"))]
        {
            println!(
                "PodDump {}\nUsage: poddump --pod FULL_POD_UID --output NEW_DIR [--request-id ID]\n\nAttempts every mapped memory range for one local Pod UID, including the sandbox; unread bytes are explicit gaps.\nRead/write parallelism, shared node budgets and durable storage are managed internally.\nEach process is captured independently, not a Pod-atomic snapshot.\nRequires the host Linux proc/cgroup mounts and ptrace access.\nExit 0 confirms complete capture and durable publication; preserve stderr on failure.",
                env!("CARGO_PKG_VERSION")
            );
        }
        #[cfg(feature = "development-options")]
        println!(
            "PodDump {} (ProcDump Linux 3.5.3 corex-derived)\nUsage: poddump --pod UUID --output NEW_DIR [OPTIONS]\n\n  --pod UUID                     Full Pod UID on the local node\n  --output NEW_DIR               New output directory\n  --format tree|json             Terminal output (default: tree)\n  --full-ids                     Show full container IDs in the tree\n  --jobs 1..16                   Concurrent process workers (default: 4)\n  --memory-mib N                 Snapshot budget (default: 512)\n  --buffer-mib N                 Per-target buffer ceiling (default: 512)\n  --writers 1..16               Independent writers (default: 2)\n  --sync-jobs 1..16             Independent finalizers (default: 1)\n  --pending-mib N               Pending output credits (default: 1024)\n  --writeback none|hint          Writeback experiment (default: none)\n  --shared-budget FILE          Shared private node-credit file\n  --global-jobs 1..16            Shared capture slots (default: 4)\n  --sync-coordination node|none Node core-fsync coordination (default:node)\n  --global-pending 1..64         Unfinished targets held through durability (default:4)\n  --buffer-policy overlap|latency Snapshot admission policy\n  --stop-timeout-ms 1..60000      Stop cap (default: 5000)\n  --trace-level stages|basic     Stage/syscall diagnostics\n  --request-id ID                Caller correlation label\n  --stream                      Capture while stopped\n  --durability batch|per-process Ready-file grouping (default: batch)\n  --census-rounds 0..8           Additional census rounds (default: 2)\n  --timeout-ms 1..3600000        Capture/write deadline (default: 60000)\n\nCompatibility: --pod-uid is an alias for --pod.\nLocal Linux cgroup v1/v2 host (containerd, CRI-O, Docker); includes sandbox processes.\nEach process is captured independently; this is not a Pod-atomic snapshot.",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(true);
    }
    let valued = [
        "--pod",
        "--pod-uid",
        "--output",
        "--jobs",
        "--buffer-mib",
        "--memory-mib",
        "--census-rounds",
        "--timeout-ms",
        "--durability",
        "--format",
        "--writers",
        "--sync-jobs",
        "--pending-mib",
        "--writeback",
        "--shared-budget",
        "--global-jobs",
        "--global-pending",
        "--sync-coordination",
        "--stop-timeout-ms",
        "--buffer-policy",
        "--trace-level",
        "--request-id",
    ];
    let mut i = 1;
    let mut keys = std::collections::BTreeSet::new();
    while i < args.len() {
        let k = &args[i];
        if !cfg!(feature = "development-options")
            && !["--pod", "--pod-uid", "--output", "--request-id"].contains(&k.as_str())
        {
            return Err(format!("unknown option: {k}; use --help"));
        }
        if !keys.insert(k) {
            return Err(format!("duplicate option: {k}"));
        }
        if k == "--stream" || k == "--full-ids" {
            i += 1;
            continue;
        }
        if !valued.contains(&k.as_str()) {
            return Err(format!("unknown option: {k}"));
        }
        if i + 1 >= args.len() || args[i + 1].starts_with("--") {
            return Err(format!("missing value: {k}"));
        }
        i += 2;
    }
    let val = |key: &str, default: &str| -> String {
        args.iter()
            .position(|x| x == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or(default.into())
    };
    if keys.contains(&String::from("--pod")) && keys.contains(&String::from("--pod-uid")) {
        return Err("use either --pod or --pod-uid, not both".into());
    }
    let format = val("--format", "tree");
    if format != "tree" && format != "json" {
        return Err("format must be tree or json".into());
    }
    let trace_level = val("--trace-level", "stages");
    if trace_level != "stages" && trace_level != "basic" {
        return Err("trace-level must be stages or basic".into());
    }
    TRACE.store(trace_level == "stages", Ordering::Relaxed);
    let full_ids = args.iter().any(|s| s == "--full-ids");
    let uid = val("--pod", &val("--pod-uid", "")).to_ascii_lowercase();
    identity::valid_uid(&uid)?;
    let out = PathBuf::from(val("--output", ""));
    if out.as_os_str().is_empty() {
        return Err("--output is required".into());
    }
    let jobs: usize = val("--jobs", "4").parse().map_err(|_| "invalid jobs")?;
    if !(1..=16).contains(&jobs) {
        return Err("jobs must be 1..16".into());
    }
    let rounds: usize = val("--census-rounds", "2")
        .parse()
        .map_err(|_| "invalid census rounds")?;
    if rounds > 8 {
        return Err("census rounds must be 0..8".into());
    }
    let timeout: u64 = val("--timeout-ms", "60000")
        .parse()
        .map_err(|_| "invalid timeout")?;
    if timeout == 0 || timeout > 3600000 {
        return Err("timeout-ms must be 1..3600000".into());
    }
    let stop_timeout = val("--stop-timeout-ms", "5000")
        .parse::<u64>()
        .map_err(|_| "invalid stop-timeout-ms")?;
    if stop_timeout == 0 || stop_timeout > 60000 {
        return Err("stop-timeout-ms must be1..60000".into());
    }
    let reader = "proc_mem";
    let limit: usize = val("--buffer-mib", "512")
        .parse()
        .map_err(|_| "invalid buffer limit")?;
    let limit = limit.checked_mul(1024 * 1024).ok_or("buffer overflow")?;
    let aggregate: usize = val("--memory-mib", "512")
        .parse()
        .map_err(|_| "invalid aggregate memory limit")?;
    let aggregate = aggregate
        .checked_mul(1024 * 1024)
        .ok_or("aggregate memory overflow")?;
    let limit = limit.min(aggregate);
    let writers: usize = val("--writers", "2")
        .parse()
        .map_err(|_| "invalid writers")?;
    let sync_jobs: usize = val("--sync-jobs", "1")
        .parse()
        .map_err(|_| "invalid sync-jobs")?;
    if !(1..=16).contains(&writers) || !(1..=16).contains(&sync_jobs) {
        return Err("writers/sync-jobs must be1..16".into());
    }
    let backlog = val("--pending-mib", "1024")
        .parse::<usize>()
        .map_err(|_| "invalid pending-mib")?
        .checked_mul(pipeline::MIB)
        .ok_or("pending-mib overflow")?;
    if backlog == 0 {
        return Err("pending-mib must be positive".into());
    }
    let writeback = val("--writeback", "none");
    if writeback != "none" && writeback != "hint" {
        return Err("writeback must be none or hint".into());
    }
    let hint = writeback == "hint";
    let shared_path = if cfg!(feature = "development-options") {
        val("--shared-budget", "")
    } else {
        shared_budget::automatic_path()?
            .to_string_lossy()
            .into_owned()
    };
    let global_jobs = val("--global-jobs", "4")
        .parse::<usize>()
        .map_err(|_| "invalid global-jobs")?;
    if !(1..=16).contains(&global_jobs) {
        return Err("global-jobs must be1..16".into());
    }
    let global_pending: usize = val("--global-pending", "4")
        .parse()
        .map_err(|_| "invalid global-pending")?;
    if !(1..=64).contains(&global_pending) {
        return Err("global-pending must be1..64".into());
    }
    let shared_init_begin = now();
    let sync_coordination = val("--sync-coordination", "node");
    if !["node", "none"].contains(&sync_coordination.as_str()) {
        return Err("sync-coordination must be node|none".into());
    }
    let shared = if shared_path.is_empty() {
        None
    } else {
        Some(shared_budget::SharedBudget::new_with_limits(
            Path::new(&shared_path),
            global_jobs,
            aggregate,
            backlog,
            global_pending,
            &engine::CancellationToken::with_timeout(timeout),
        )?)
    };
    let shared_init_end = now();
    let buffer_policy = val(
        "--buffer-policy",
        if cfg!(feature = "development-options") {
            "overlap"
        } else {
            "latency"
        },
    );
    if buffer_policy != "overlap" && buffer_policy != "latency" {
        return Err("buffer-policy must be overlap or latency".into());
    }
    // Two in-flight snapshots can overlap only if neither monopolizes the pool.
    // A large target uses bounded streaming; the stop deadline still applies.
    let admission_ceiling = if buffer_policy == "overlap" && (jobs > 1 || shared.is_some()) {
        limit.min(aggregate / 2)
    } else {
        limit
    };
    let stream = args.iter().any(|s| s == "--stream");
    let durability = val("--durability", "batch");
    if durability != "batch" && durability != "per-process" {
        return Err("invalid durability".into());
    }

    let request_id = val(
        "--request-id",
        &format!("{}-{}", std::process::id(), total_begin),
    );
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
    {
        return Err("request-id must contain1..128 ASCII letters/digits or -_.:".into());
    }
    let root = identity::resolve(&uid)?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e| e.to_string())?;
    let (mut initial, mut initial_errors) = identity::census(&root, &uid)?;
    let keys = |ids: &Vec<identity::Identity>| {
        ids.iter()
            .map(|x| (x.pid, x.starttime, x.cgroup.clone()))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let mut initial_stable = false;
    for _ in 0..3 {
        let (next, errors) = identity::census(&root, &uid)?;
        initial_errors.extend(errors);
        if keys(&initial) == keys(&next) {
            initial_stable = true;
            break;
        }
        let mut union = std::collections::BTreeMap::new();
        for id in initial.iter().chain(next.iter()) {
            union.insert((id.pid, id.starttime), id.clone());
        }
        initial = union.into_values().collect();
    }
    if !initial_stable {
        initial_errors.push(json!({"kind":"initial_census_unstable","error":"bounded initial census did not stabilize; union retained"}));
    }
    let discovery_complete = now();
    let discovery_s = total_timer.elapsed().as_secs_f64();
    if initial.is_empty() {
        return Err("Pod cgroup has no live processes".into());
    }
    let requested_out = out;
    let output_guard = safe_output::OutputDir::create(&requested_out)
        .map_err(|e| format!("create pinned private output: {e}"))?;
    let out = output_guard.path();
    let reserve_begin = now();
    let reserve = out.join("manifest_reserve");
    let mut reserve_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&reserve)
        .map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    let reserve_bytes = (initial.len() as i64)
        .saturating_mul(65536)
        .saturating_add(1048576);
    let rc = unsafe { libc::posix_fallocate(reserve_file.as_raw_fd(), 0, reserve_bytes) };
    if rc != 0 {
        if matches!(rc, libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL) {
            let chunk = [0u8; 65536];
            let mut rem = reserve_bytes as usize;
            while rem > 0 {
                let n = rem.min(chunk.len());
                reserve_file
                    .write_all(&chunk[..n])
                    .map_err(|e| format!("cannot reserve manifest space: {e}"))?;
                rem -= n;
            }
        } else {
            return Err(format!(
                "cannot reserve manifest space: {}",
                std::io::Error::from_raw_os_error(rc)
            ));
        }
    }
    reserve_file.sync_all().map_err(|e| e.to_string())?;
    // Retain reserve ownership until verified cleanup.
    let reserve_end = now();
    let begin = total_begin;
    let timer = total_timer;
    let results = Mutex::new(Vec::<Value>::new());
    let mut descriptor_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut descriptor_limit) } != 0 {
        return Err("cannot read descriptor limit".into());
    }
    let pending_fd_limit = descriptor_limit.rlim_cur.saturating_sub(64).min(256) as usize;
    if pending_fd_limit == 0 {
        return Err("at least65 file descriptors are required".into());
    }
    let durability_barriers = Mutex::new(Vec::<Value>::new());
    let budget = pipeline::Budget::new(aggregate, backlog, pending_fd_limit);
    let (write_tx, write_rx) = std::sync::mpsc::channel::<WriteWork>();
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<SyncWork>();
    let write_rx = Mutex::new(write_rx);
    let sync_rx = Mutex::new(sync_rx);
    let mut seen = std::collections::BTreeSet::new();
    let mut batch = initial.clone();
    let mut censuses = vec![json!({"stage":"initial","errors":initial_errors})];
    std::thread::scope(|scope_threads| {
        for _ in 0..sync_jobs {
            scope_threads.spawn(|| {
                loop {
                    let wait_begin=now();
                    let Some(mut group) = receive_ready(&sync_rx, if durability == "batch" {8} else {1}) else { break };
                    group[0].row["details"]["finalizer_receive_wait_ns"]=json!(now()-wait_begin);
                    let begin = now();
                    let count = group.len();
                    for mut work in group {
                        work.row["details"]["finalizer_received_ns"]=json!(now());
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let permit = if sync_coordination == "node" {
                                shared.as_ref().map(|b| b.acquire_sync()).transpose()
                            } else { Ok(None) };
                            match permit {
                                Ok(permit) => {
                                    if let Some(p) = &permit { work.row["details"]["node_sync_admission"] = p.diagnostics(); }
                                    finalize_core(&out, &mut work.row, Some(work.file));
                                }
                                Err(e) => {
                                    work.row["status"] = json!("failed");work.row["failure_stage"] = json!("node_sync_admission");work.row["error"] = json!(e);work.row["end_ns"] = json!(now());
                                }
                            }
                        }));
                        if result.is_err() { work.row["status"]=json!("failed"); work.row["error"]=json!("finalizer panicked"); CANCEL.store(true,Ordering::Relaxed); }
                        results.lock().unwrap_or_else(|e|e.into_inner()).push(work.row);
                    }
                    durability_barriers.lock().unwrap().push(json!({"processes":count,"begin_ns":begin,"end_ns":now(),"kind":"ready_files"}));
                }
            });
        }
        for _ in 0..writers {
            let tx = sync_tx.clone();
            let write_rx = &write_rx;
            let out = &out;
            let results = &results;
            scope_threads.spawn(move || {
                loop {
                    let wait_begin = now();
                    let Ok(mut work) = write_rx.lock().unwrap().recv() else {
                        break;
                    };
                    work.captured.details["writer_received_ns"] = json!(now());
                    work.captured.details["writer_receive_wait_ns"] = json!(now() - wait_begin);
                    let path = out.join(work.row["partial_file"].as_str().unwrap());
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        work.captured.write(&path, &work.token, hint)
                    }));
                    work.lease.release_memory(); // Captured dropped its snapshot, including on error/panic.
                    if let Some(shared) = work.shared.as_mut() {
                        if let Err(e) = shared.release_memory() {
                            work.row["status"] = json!("failed");
                            work.row["error"] = json!(e);
                            results
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(work.row);
                            CANCEL.store(true, Ordering::Relaxed);
                            continue;
                        }
                    }
                    match result {
                        Ok(Ok((details, file))) => {
                            work.row["details"] = details;
                            if let Err(e) = tx.send(SyncWork {
                                row: work.row,
                                file,
                                _lease: work.lease,
                                _shared: work.shared,
                            }) {
                                let mut row = e.0.row;
                                row["status"] = json!("failed");
                                row["error"] = json!("finalizer unavailable");
                                results.lock().unwrap_or_else(|e| e.into_inner()).push(row);
                                CANCEL.store(true, Ordering::Relaxed);
                            }
                        }
                        failure => {
                            work.row["status"] = json!("failed");
                            work.row["failure_stage"] = json!("write");
                            work.row["end_ns"] = json!(now());
                            work.row["error"] = json!(match failure {
                                Ok(Err(e)) => e.to_string(),
                                _ => {
                                    CANCEL.store(true, Ordering::Relaxed);
                                    "writer panicked".into()
                                }
                            });
                            results
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(work.row);
                        }
                    }
                }
            });
        }
        drop(sync_tx);
        for round in 0..=rounds {
            let preflight_begin = now();
            let queue = Mutex::new(
                batch
                    .iter()
                    .map(|id| pipeline::Request::new(id.clone(), corex::estimate(id, &uid)))
                    .collect::<std::collections::VecDeque<_>>(),
            );
            let preflight_end = now();
            std::thread::scope(|readers| {
                for _ in 0..jobs {
                    readers.spawn(|| {
                        while let Some((request,lease,buffered))=budget.next(&queue,admission_ceiling,stream) {
                            let admission=request.diagnostics();
                            let id=request.id;
                            let started=now(); let file=out.join(format!("{}_{}.core.partial",id.pid,id.starttime));
                            let token=engine::CancellationToken::with_timeout(timeout);
                            let mut row=json!({"identity":id,"status":"failed","round":round,"begin_ns":started,"preflight_begin_ns":preflight_begin,"preflight_end_ns":preflight_end,"partial_file":file.file_name().unwrap().to_string_lossy()});
                            row["admission"]=admission;
                            row["buffer_mode_reason"]=json!(if stream{"explicit_stream"}else if buffered{"fits_admission_ceiling"}else if request.estimate>aggregate{"exceeds_total_snapshot_budget"}else{"exceeds_overlap_or_target_ceiling"});
                            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                if token.is_cancelled() { return Err(corex::CorexError::Cancelled); }
                                if let Some(e)=request.error { return Err(corex::CorexError::InvalidData(e)); }
                                let mut shared = match &shared { Some(b)=>Some(b.acquire(if buffered{request.estimate}else{0},request.estimate,&token).map_err(corex::CorexError::InvalidData)?),None=>None };
                                id.verify(&uid).map_err(corex::CorexError::InvalidData)?;
                                let f=OpenOptions::new().create_new(true).write(true).mode(0o600).open(&file).map_err(|e|corex::CorexError::io("create core",file.display().to_string(),e))?;
                                let stop_token=token.capped(stop_timeout);
                                let captured=corex::capture(&id,&uid,&file,f,&stop_token,request.estimate,!buffered,&reader,request.estimate)?;
                                if let Some(s)=shared.as_mut(){s.release_capture().map_err(corex::CorexError::InvalidData)?;}
                                Ok((captured,shared))
                            }));
                            match result {
                                Ok(Ok((captured,shared)))=> {
                                    row["status"]=json!("captured_pending_durability");row["details"]=captured.details.clone();row["worker_complete_ns"]=json!(now());
                                    if let Some(s)=shared.as_ref(){row["shared_admission"]=s.diagnostics();}
                                    if let Err(e)=write_tx.send(WriteWork {row,captured,lease,shared,token}) {
                                        let mut row=e.0.row; row["status"]=json!("failed"); row["error"]=json!("writer unavailable");
                                        results.lock().unwrap_or_else(|e|e.into_inner()).push(row);CANCEL.store(true,Ordering::Relaxed);
                                    }
                                }
                                failure=> {
                                    row["failure_stage"]=json!("capture");row["end_ns"]=json!(now());
                                    row["error"]=json!(match failure {Ok(Err(e))=>{if let Some(details)=e.coverage_details(){row["details"]=details;}e.to_string()},_=>{CANCEL.store(true,Ordering::Relaxed);"capture worker panicked".into()}});
                                    results.lock().unwrap_or_else(|e|e.into_inner()).push(row);
                                }
                            }
                        }
                    });
                }
            });
            for id in &batch {
                seen.insert((id.pid, id.starttime));
            }
            let census_wait_begin_ns = now();
            budget.wait_idle();
            let census_wait_complete_ns = now();
            match identity::census(&root, &uid) {
                Ok((census, errors)) => {
                    censuses.push(
                    json!({"round":round,"time_ns":now(),"census_wait_begin_ns":census_wait_begin_ns,"census_wait_complete_ns":census_wait_complete_ns,"processes":census,"errors":errors}),
                );
                    batch = census
                        .into_iter()
                        .filter(|id| !seen.contains(&(id.pid, id.starttime)))
                        .collect();
                    if batch.is_empty() || CANCEL.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(e) => {
                    censuses.push(json!({"round":round,"time_ns":now(),"error":e}));
                    break;
                }
            }
        }
        drop(write_tx);
    });
    let results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    let durability_barriers = durability_barriers.into_inner().unwrap();
    let durability_begin_ns = durability_barriers
        .iter()
        .filter_map(|x| x["begin_ns"].as_u64())
        .min();
    let durability_complete_ns = durability_barriers
        .iter()
        .filter_map(|x| x["end_ns"].as_u64())
        .max();
    let cancel_requested = CANCEL.load(Ordering::Relaxed);
    let mut success = !cancel_requested
        && results.iter().all(|x| x["status"] == "complete")
        && batch.is_empty()
        && !censuses.iter().any(|x| {
            x.get("error").is_some()
                || x.get("errors")
                    .and_then(|e| e.as_array())
                    .is_some_and(|e| !e.is_empty())
        });
    let mut manifest = json!({"schema":2,"durability_contract":"Manifest status describes collection completeness; successful process exit is also required to confirm final manifest/output/parent directory persistence. On persistence failure preserve stderr emergency_manifest and nonzero exit.","build_provenance":serde_json::from_str::<Value>(include_str!("../build_provenance.json")).unwrap(),"timebase":"CLOCK_MONOTONIC nanoseconds","wall_time_unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),"tool":"PodDump","version":env!("CARGO_PKG_VERSION"),"upstream":"ProcDump-Linux 3.5.3 8e34cbfaf57b38ca6b73160031e360edcbe6b9c1","pod_uid":uid,"cgroup_root":root,"boot_id":boot.trim(),"node":fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),"collection_policy":"all_mapped_proc_mem","durability":durability,"pending_fd_limit":pending_fd_limit,"durability_barriers":durability_barriers,"durability_begin_ns":durability_begin_ns,"durability_complete_ns":durability_complete_ns,"reader":reader,"initial_census_stable":initial_stable,"timeout_per_process_ms":timeout,"timeout_scope":"cooperative capture/write deadline; final durability attempts finish after capture; kernel I/O may block","cancel_requested":cancel_requested,"jobs":jobs,"buffer_ceiling_per_target":limit,"writers":writers,"sync_jobs":sync_jobs,"writeback_policy":writeback,"pipeline":budget.metrics(),"stream_scratch_limit_bytes":jobs*pipeline::MIB,"stream":stream,"begin_ns":begin,"discovery_complete_ns":discovery_complete,"discovery_s":discovery_s,"memory_budget_bytes":aggregate,"end_ns":now(),"elapsed_s":timer.elapsed().as_secs_f64(),"initial":initial,"censuses":censuses,"uncaptured_new":batch,"processes":results,"status":if success{"complete"}else{"partial"},"atomicity":"Each process stopped independently; shared mappings may change externally; not a Pod-atomic snapshot","registers":"GP and FP regsets (upstream), x86 XSAVE additionally captured"});
    manifest["request_id"] = json!(request_id);
    manifest["global_pending_limit"] = json!(global_pending);
    manifest["node_sync_coordination"] = json!({"mode":sync_coordination,"core_fsync_limit":if sync_coordination=="node" && shared.is_some(){Some(1)}else{None},"participation":"all callers must use the same shared budget path and support this coordination; older binaries do not participate","cancellation":"final durable attempts may wait beyond capture cancellation, like fsync itself; no target is stopped while waiting"});
    manifest["clock_correlation"] = json!({"start":clock_start,"end":clock_pair()});
    manifest["scheduling"] = json!({"shared_budget":shared_path,"global_jobs":global_jobs,"buffer_policy":buffer_policy,"admission_ceiling_bytes":admission_ceiling,"stop_timeout_ms":stop_timeout,"trace_level":trace_level,"shared_init_begin_ns":shared_init_begin,"shared_init_end_ns":shared_init_end,"manifest_reserve_begin_ns":reserve_begin,"manifest_reserve_end_ns":reserve_end,"final_pipeline_join_ns":now()});
    if let Err(e) = output_guard.verify_location() {
        manifest["status"] = json!("partial");
        manifest["output_path_error"] = json!(e.to_string());
        success = false;
    }
    success = cleanup_reserve(&reserve, &reserve_file, &mut manifest) && success;
    if let Err(e) = atomic_json(&out.join("manifest.json"), &manifest, output_guard.parent())
        .and_then(|_| output_guard.verify_location().map_err(|e| e.to_string()))
    {
        eprintln!(
            "{}",
            json!({"manifest_write_error":e,"emergency_manifest":manifest})
        );
        return Err(e);
    };
    let elapsed_s = timer.elapsed().as_secs_f64();
    if format == "json" {
        println!(
            "{}",
            json!({"status":manifest["status"],"manifest":requested_out.join("manifest.json"),"elapsed_s":elapsed_s})
        );
    } else {
        println!(
            "{}",
            cli_output::tree(&manifest, &requested_out, elapsed_s, full_ids)
        );
    }
    Ok(success)
}
fn main() {
    unsafe {
        libc::signal(libc::SIGINT, signal as *const () as usize);
        libc::signal(libc::SIGTERM, signal as *const () as usize);
    }
    match run() {
        Ok(true) => (),
        Ok(false) => std::process::exit(2),
        Err(e) => {
            eprintln!("PodDump: {e}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;
    #[test]
    fn reserve_cleanup_error_does_not_skip_manifest_publication() {
        let d = directory();
        let reserve = d.join("manifest_reserve");
        let owned = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&reserve)
            .unwrap();
        fs::rename(&reserve, d.join("saved_reserve")).unwrap();
        fs::create_dir(&reserve).unwrap();
        let mut manifest = json!({"status":"complete"});
        assert!(!cleanup_reserve(&reserve, &owned, &mut manifest));
        atomic_json(
            &d.join("manifest.json"),
            &manifest,
            &File::open(d.parent().unwrap()).unwrap(),
        )
        .unwrap();
        let persisted: Value =
            serde_json::from_slice(&fs::read(d.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(persisted["status"], "partial");
        assert_eq!(persisted["cleanup_errors"][0]["stage"], "reserve_cleanup");
        fs::remove_dir_all(d).unwrap();
    }
    fn directory() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "poddump_durability_{}_{}",
            std::process::id(),
            now()
        ));
        fs::create_dir(&p).unwrap();
        p
    }
    fn pending(name: &str) -> Value {
        json!({"status":"captured_pending_durability","partial_file":name,"details":{"durable_ns":null}})
    }
    #[test]
    fn pending_core_becomes_durable_before_publication() {
        let d = directory();
        let p = d.join("1.core.partial");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
            .unwrap();
        file.write_all(b"immutable captured bytes").unwrap();
        let mut row = pending("1.core.partial");
        finalize_core(&d, &mut row, Some(file));
        assert_eq!(row["status"], "complete");
        assert!(row["details"]["durable_ns"].as_u64().unwrap() > 0);
        assert_eq!(
            fs::read(d.join("1.core")).unwrap(),
            b"immutable captured bytes"
        );
        assert!(!p.exists());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn symlink_replacement_is_failed_and_never_published() {
        use std::os::unix::fs::symlink;
        let d = directory();
        let target = d.join("unrelated");
        fs::write(&target, b"preserve").unwrap();
        let file = File::create(d.join("original")).unwrap();
        symlink(&target, d.join("1.core.partial")).unwrap();
        let mut row = pending("1.core.partial");
        finalize_core(&d, &mut row, Some(file));
        assert_eq!(row["status"], "failed");
        assert_eq!(row["failure_stage"], "durability");
        assert!(row["details"]["durable_ns"].is_null());
        assert!(!d.join("1.core").exists());
        assert_eq!(fs::read(target).unwrap(), b"preserve");
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn finalizer_preserves_existing_final_core() {
        let d = directory();
        let p = d.join("1.core.partial");
        let final_path = d.join("1.core");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
            .unwrap();
        file.write_all(b"captured").unwrap();
        fs::write(&final_path, b"preserve").unwrap();
        let mut row = pending("1.core.partial");
        finalize_core(&d, &mut row, Some(file));
        assert_eq!(row["status"], "failed");
        assert_eq!(fs::read(final_path).unwrap(), b"preserve");
        assert_eq!(fs::read(p).unwrap(), b"captured");
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn manifest_publication_preserves_existing_final_file() {
        let d = directory();
        let path = d.join("manifest.json");
        fs::write(&path, b"preserve").unwrap();
        let result = atomic_json(
            &path,
            &json!({"status":"complete"}),
            &File::open(d.parent().unwrap()).unwrap(),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), b"preserve");
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn failed_capture_is_not_promoted_by_durability() {
        let d = directory();
        fs::write(d.join("1.core.partial"), b"incomplete").unwrap();
        let mut row =
            json!({"status":"failed","partial_file":"1.core.partial","error":"memory read failed"});
        let original = row.clone();
        finalize_core(&d, &mut row, None);
        assert_eq!(row, original);
        assert!(!d.join("1.core").exists());
        fs::remove_dir_all(d).unwrap();
    }
}

#[cfg(test)]
mod scheduling_regressions {
    use super::*;
    #[test]
    fn legacy_split_receive_has_a_reproducible_lock_cycle() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = std::sync::Arc::new(Mutex::new(rx));
        tx.send(1).unwrap();
        // A has already taken the only backlog credit and released the receiver.
        let held_credit = rx.lock().unwrap().recv().unwrap();
        assert_eq!(held_credit, 1);
        let (locked, ready) = std::sync::mpsc::channel();
        let b_rx = rx.clone();
        let b = std::thread::spawn(move || {
            let guard = b_rx.lock().unwrap();
            locked.send(()).unwrap();
            guard.recv()
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        // In the legacy code A now blocks here, while B waits for a producer
        // which cannot be admitted until A fsyncs and releases its credit.
        assert!(matches!(
            rx.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        // Close only the synthetic test channel to break the reproduced cycle.
        drop(tx);
        assert!(b.join().unwrap().is_err());
    }
    #[test]
    fn two_finalizers_single_pending_credit_cannot_deadlock() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = std::sync::Arc::new(Mutex::new(rx));
        let (done, observed) = std::sync::mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..2 {
            let rx = rx.clone();
            let done = done.clone();
            workers.push(std::thread::spawn(move || {
                while let Some(group) = receive_ready(&rx, 8) {
                    for item in group {
                        done.send(item).unwrap();
                    }
                }
            }));
        }
        // Next capture is admitted only after the prior finalizer returns credit.
        for i in 0..100 {
            tx.send(i).unwrap();
            assert_eq!(
                observed
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap(),
                i
            );
        }
        drop(tx);
        for worker in workers {
            worker.join().unwrap();
        }
    }
    #[test]
    fn stop_deadline_never_restarts_expired_request() {
        let original = engine::CancellationToken::with_timeout(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(original.capped(5000).timed_out());
        let original = engine::CancellationToken::with_timeout(5000);
        let stopped = original.capped(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(!original.timed_out());
        assert!(stopped.timed_out());
        original.cancel();
        assert!(stopped.is_cancelled());
    }
}

#[cfg(test)]
mod coverage_publication_regressions {
    use super::*;
    #[test]
    fn durable_elf_with_read_gaps_is_partial_and_keeps_partial_name() {
        let d =
            std::env::temp_dir().join(format!("poddump_read_gap_{}_{}", std::process::id(), now()));
        fs::create_dir(&d).unwrap();
        let p = d.join("1_1.core.partial");
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
            .unwrap();
        f.write_all(b"partial ELF test payload").unwrap();
        let mut row = json!({"status":"captured_pending_durability","partial_file":"1_1.core.partial","details":{"coverage":{"complete":false,"failed_bytes":4096}}});
        finalize_core(&d, &mut row, Some(f));
        assert_eq!(row["status"], "partial");
        assert!(p.exists());
        assert!(!d.join("1_1.core").exists());
        assert!(row["details"]["durable_ns"].is_number());
        fs::remove_file(p).unwrap();
        fs::remove_dir(d).unwrap();
    }
}
