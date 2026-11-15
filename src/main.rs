#![recursion_limit = "256"]
mod build_info;
mod cli_output;
mod corex;
mod identity;
mod pipeline;
mod pod_lock;
mod rate_limit;
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
        // Read via the held inode only when streaming mode did not already compute
        // the digest inline during write. Evict finished core pages from host cache.
        use std::os::fd::AsRawFd;
        if row["details"]["core_sha256"].is_null() {
            let hash_begin = now();
            let input = File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|e| format!("open captured inode for digest: {e}"))?;
            row["details"]["core_sha256"] = json!(
                build_info::hash_sparse_file(input)
                    .map_err(|e| format!("hash captured core: {e}"))?
            );
            row["details"]["hash_mode"] = json!("post_sync_fd_read");
            row["details"]["hash_begin_ns"] = json!(hash_begin);
            row["details"]["hash_end_ns"] = json!(now());
        }
        let _ = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
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
                row["error"] = json!(
                    if row["details"]["coverage"]["stop_timeout_reached"].as_bool() == Some(true) {
                        "stop timeout reached before all mapped bytes were read; consult coverage ledger"
                    } else {
                        "mapped bytes are missing; consult coverage ledger"
                    }
                );
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
fn empty_census_error(errors: &[serde_json::Value]) -> String {
    if errors.is_empty() {
        "Pod cgroup has no live processes".into()
    } else {
        serde_json::json!({"stage":"initial_census", "error":"no_verified_process_identities",
                           "census_errors":errors})
        .to_string()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Help,
    Version,
    Run { uid: String, output: PathBuf },
}

fn take_arg_value(
    flag: &str,
    inline: Option<&str>,
    next: Option<&String>,
) -> Result<(String, usize), String> {
    match inline {
        Some(value) => {
            if value.is_empty() {
                return Err(format!("missing value: {flag}"));
            }
            if value.starts_with('-') {
                return Err(format!("invalid value for {flag}: {value}"));
            }
            Ok((value.to_string(), 1))
        }
        None => match next {
            Some(value) if !value.is_empty() && !value.starts_with('-') => Ok((value.clone(), 2)),
            Some(value) if value.starts_with('-') => {
                Err(format!("invalid value for {flag}: {value}"))
            }
            _ => Err(format!("missing value: {flag}")),
        },
    }
}

fn parse_cli_args(args: &[String]) -> Result<CliAction, String> {
    if args.len() <= 1 {
        return Ok(CliAction::Help);
    }
    if args.len() == 2 {
        match args[1].as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            _ => {}
        }
    }
    let mut pod: Option<String> = None;
    let mut output: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        let (flag, inline) = match arg.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (arg.as_str(), None),
        };
        match flag {
            "--pod" => {
                if pod.is_some() {
                    return Err("duplicate option: --pod".into());
                }
                let (val, step) = take_arg_value("--pod", inline, args.get(i + 1))?;
                pod = Some(val);
                i += step;
            }
            "--output" => {
                if output.is_some() {
                    return Err("duplicate option: --output".into());
                }
                let (val, step) = take_arg_value("--output", inline, args.get(i + 1))?;
                output = Some(PathBuf::from(val));
                i += step;
            }
            _ => return Err(format!("unknown option: {arg}; use --help")),
        }
    }
    let uid = pod.ok_or("--pod is required")?.to_ascii_lowercase();
    identity::valid_uid(&uid)?;
    let output = output.ok_or("--output is required")?;
    Ok(CliAction::Run { uid, output })
}

fn print_help() {
    println!(
        "PodDump {}\nUsage: poddump --pod FULL_POD_UID --output NEW_DIR\n\nCaptures all mapped memory ranges for every container and sandbox process of one local Pod into ELF Core files.\nAll parallelism, node shared budgets, sparse zero-skip, inline SHA-256 and durable publication defaults are managed internally.\nEach process is stopped briefly and captured independently; this is not a Pod-atomic snapshot.\nRequires host Linux proc/cgroup mounts and ptrace access.\nOptions:\n  --pod FULL_POD_UID   Full Pod UID on the local node (required)\n  --output NEW_DIR     New private output directory (required)\n  -h, --help           Show this help and exit\n  -V, --version        Show version and exit\nExit codes: 0 complete, 1 usage/runtime error, 2 partial capture, 3 active duplicate lock.",
        env!("CARGO_PKG_VERSION")
    );
}

fn run() -> Result<i32, String> {
    let clock_start = clock_pair();
    let total_begin = now();
    let total_timer = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let (uid, out) = match parse_cli_args(&args)? {
        CliAction::Help => {
            print_help();
            return Ok(0);
        }
        CliAction::Version => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        CliAction::Run { uid, output } => (uid, output),
    };
    let trace_level = "stages";
    TRACE.store(true, Ordering::Relaxed);
    let jobs: usize = 4;
    let rounds: usize = 2;
    let timeout: u64 = 60_000;
    let stop_timeout: u64 = 5_000;
    let reader = "proc_mem";
    let limit: usize = 512 * 1024 * 1024;
    let aggregate: usize = 512 * 1024 * 1024;
    let writers: usize = 2;
    let sync_jobs: usize = 1;
    let backlog: usize = 1024 * pipeline::MIB;
    let writeback = "hint";
    let hint = true;
    let shared_path = shared_budget::automatic_path()?
        .to_string_lossy()
        .into_owned();
    let global_jobs: usize = 4;
    let global_pending: usize = 4;
    let shared_init_begin = now();
    let sync_coordination = "node";
    let shared = shared_budget::SharedBudget::new_with_limits(
        Path::new(&shared_path),
        global_jobs,
        aggregate,
        backlog,
        global_pending,
        &engine::CancellationToken::with_timeout(timeout),
    )?;
    let shared_init_end = now();
    let buffer_policy = "latency";
    let admission_ceiling = limit;
    let stream = false;
    let durability = "batch";
    let request_id = format!("{}-{}", std::process::id(), total_begin);
    let wait_timeout: u64 = 60_000;
    let lock_path = shared_budget::automatic_path()?;
    let pod_lease = match pod_lock::acquire(
        lock_path.parent().ok_or("no runtime directory")?,
        &uid,
        &request_id,
        &out,
        wait_timeout,
        || CANCEL.load(Ordering::Relaxed),
    )? {
        pod_lock::Admission::Acquired(lease) => lease,
        pod_lock::Admission::Linked(info) => {
            println!("{}", info);
            return Ok(3);
        }
    };
    // Discovery happens after admission, so queued requests do not use stale PIDs.
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
        return Err(empty_census_error(&initial_errors));
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
        return Err("at least 65 file descriptors are required".into());
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
                            match shared.acquire_sync() {
                                Ok(permit) => {
                                    work.row["details"]["node_sync_admission"] = permit.diagnostics();
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
                    if let Some(shared) = work.shared.as_mut()
                        && let Err(e) = shared.release_memory()
                    {
                        work.row["status"] = json!("failed");
                        work.row["error"] = json!(e);
                        results
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(work.row);
                        CANCEL.store(true, Ordering::Relaxed);
                        continue;
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
                                let mut shared = Some(
                                    shared
                                        .acquire(
                                            if buffered { request.estimate } else { 0 },
                                            request.estimate,
                                            &token,
                                        )
                                        .map_err(corex::CorexError::InvalidData)?,
                                );
                                id.verify(&uid).map_err(corex::CorexError::InvalidData)?;
                                let f=OpenOptions::new().create_new(true).write(true).mode(0o600).open(&file).map_err(|e|corex::CorexError::io("create core",file.display().to_string(),e))?;
                                let stop_token=token.capped(stop_timeout);
                                let output_ceiling=backlog.max(request.estimate);
                                let captured=corex::capture(&id,&uid,&file,f,&stop_token,request.estimate,!buffered,reader,output_ceiling)?;
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
                                    if !file.exists() {
                                        row.as_object_mut().unwrap().remove("partial_file");
                                    }
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
    let mut manifest = json!({"schema":2,"durability_contract":"Manifest status describes collection completeness; successful process exit is also required to confirm final manifest/output/parent directory persistence. On persistence failure preserve stderr emergency_manifest and nonzero exit.","build_provenance":build_info::current(),"timebase":"CLOCK_MONOTONIC nanoseconds","wall_time_unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),"tool":"PodDump","version":env!("CARGO_PKG_VERSION"),"upstream":"ProcDump-Linux 3.5.3 8e34cbfaf57b38ca6b73160031e360edcbe6b9c1","pod_uid":uid,"cgroup_root":root,"boot_id":boot.trim(),"node":fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),"collection_policy":"all_mapped_proc_mem","durability":durability,"pending_fd_limit":pending_fd_limit,"durability_barriers":durability_barriers,"durability_begin_ns":durability_begin_ns,"durability_complete_ns":durability_complete_ns,"reader":reader,"initial_census_stable":initial_stable,"timeout_per_process_ms":timeout,"timeout_scope":"cooperative capture/write deadline; final durability attempts finish after capture; kernel I/O may block","cancel_requested":cancel_requested,"jobs":jobs,"buffer_ceiling_per_target":limit,"writers":writers,"sync_jobs":sync_jobs,"writeback_policy":writeback,"pipeline":budget.metrics(),"stream_scratch_limit_bytes":jobs*pipeline::MIB,"stream":stream,"begin_ns":begin,"discovery_complete_ns":discovery_complete,"discovery_s":discovery_s,"memory_budget_bytes":aggregate,"end_ns":now(),"elapsed_s":timer.elapsed().as_secs_f64(),"initial":initial,"censuses":censuses,"uncaptured_new":batch,"processes":results,"status":if success{"complete"}else{"partial"},"atomicity":"Each process stopped independently; shared mappings may change externally; not a Pod-atomic snapshot","registers":"GP and FP regsets (upstream), x86 XSAVE additionally captured"});
    manifest["request_id"] = json!(request_id);
    manifest["global_pending_limit"] = json!(global_pending);
    manifest["pod_admission"] = pod_lease.diagnostics.clone();
    manifest["node_sync_coordination"] = json!({"mode":sync_coordination,"core_fsync_limit":Some(1),"participation":"all callers must use the same shared budget path and support this coordination; older binaries do not participate","cancellation":"final durable attempts may wait beyond capture cancellation, like fsync itself; no target is stopped while waiting"});
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
    println!(
        "{}",
        cli_output::tree(&manifest, &requested_out, elapsed_s, false)
    );
    Ok(if success { 0 } else { 2 })
}
fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}
fn main() {
    install_signal_handlers();
    match run() {
        Ok(code) => std::process::exit(code),
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
        assert_eq!(row["details"]["hash_mode"], "post_sync_fd_read");
        assert_eq!(
            fs::read(d.join("1.core")).unwrap(),
            b"immutable captured bytes"
        );
        assert!(!p.exists());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn finalize_core_preserves_inline_sha256_without_reread() {
        let d = directory();
        let p = d.join("2.core.partial");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
            .unwrap();
        file.write_all(b"inline digest payload").unwrap();
        let sentinel = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut row = json!({
            "status":"captured_pending_durability",
            "partial_file":"2.core.partial",
            "details":{
                "durable_ns":null,
                "core_sha256":sentinel,
                "hash_mode":"inline_write",
                "hash_begin_ns":10,
                "hash_end_ns":20
            }
        });
        finalize_core(&d, &mut row, Some(file));
        assert_eq!(row["status"], "complete");
        assert_eq!(row["details"]["core_sha256"], sentinel);
        assert_eq!(row["details"]["hash_mode"], "inline_write");
        assert!(d.join("2.core").is_file());
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

    #[test]
    fn failed_capture_unlinks_owned_empty_partial_file() {
        let d = std::env::temp_dir().join(format!(
            "poddump_failed_partial_{}_{}",
            std::process::id(),
            now()
        ));
        fs::create_dir(&d).unwrap();
        let p = d.join("999999_1.core.partial");
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
            .unwrap();
        let id = identity::Identity {
            pid: 999999,
            starttime: 1,
            cgroup: String::new(),
            container_id: String::new(),
            state: 'R',
        };
        let token = engine::CancellationToken::with_timeout(1000);
        let result = corex::capture(
            &id,
            "00000000-0000-0000-0000-000000000000",
            &p,
            f,
            &token,
            4096,
            false,
            "proc_mem",
            4096,
        );
        assert!(result.is_err());
        assert!(!p.exists());
        fs::remove_dir(d).unwrap();
    }

    #[test]
    fn identity_failures_are_not_reported_as_an_empty_pod() {
        assert_eq!(empty_census_error(&[]), "Pod cgroup has no live processes");
        let errors = [json!({"pid":17,"error":"container identity unavailable"})];
        let report: serde_json::Value = serde_json::from_str(&empty_census_error(&errors)).unwrap();
        assert_eq!(report["error"], "no_verified_process_identities");
        assert_eq!(report["census_errors"][0]["pid"], 17);
    }

    #[test]
    fn failed_capture_cleanup_reports_replaced_file_without_removing_it() {
        let d =
            std::env::temp_dir().join(format!("poddump_cleanup_{}_{}", std::process::id(), now()));
        fs::create_dir(&d).unwrap();
        let p = d.join("999999_1.core.partial");
        let owned = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&p)
            .unwrap();
        fs::rename(&p, d.join("owned_saved")).unwrap();
        fs::write(&p, b"unrelated; preserve").unwrap();
        let id = identity::Identity {
            pid: 999999,
            starttime: 1,
            cgroup: String::new(),
            container_id: String::new(),
            state: 'R',
        };
        let token = engine::CancellationToken::with_timeout(1000);
        let error = match corex::capture(
            &id,
            "00000000-0000-0000-0000-000000000000",
            &p,
            owned,
            &token,
            4096,
            false,
            "proc_mem",
            4096,
        ) {
            Err(error) => error,
            Ok(_) => panic!("invalid identity unexpectedly captured"),
        };
        assert!(error.to_string().contains("remove failed capture"));
        assert_eq!(fs::read(&p).unwrap(), b"unrelated; preserve");
        fs::remove_dir_all(d).unwrap();
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn accepts_minimal_cli_contract() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        assert_eq!(parse_cli_args(&s(&["poddump"])).unwrap(), CliAction::Help);
        assert_eq!(
            parse_cli_args(&s(&["poddump", "-h"])).unwrap(),
            CliAction::Help
        );
        assert_eq!(
            parse_cli_args(&s(&["poddump", "--help"])).unwrap(),
            CliAction::Help
        );
        assert_eq!(
            parse_cli_args(&s(&["poddump", "-V"])).unwrap(),
            CliAction::Version
        );
        assert_eq!(
            parse_cli_args(&s(&["poddump", "--version"])).unwrap(),
            CliAction::Version
        );
        assert_eq!(
            parse_cli_args(&s(&[
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/pod_1"
            ]))
            .unwrap(),
            CliAction::Run {
                uid: uid.to_string(),
                output: PathBuf::from("/evidence/pod_1"),
            }
        );
        assert_eq!(
            parse_cli_args(&s(&[
                "poddump",
                "--output=/evidence/pod_2",
                &format!("--pod={}", uid.to_ascii_uppercase())
            ]))
            .unwrap(),
            CliAction::Run {
                uid: uid.to_string(),
                output: PathBuf::from("/evidence/pod_2"),
            }
        );
    }

    #[test]
    fn rejects_removed_duplicate_and_flag_like_arguments() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        let rejected = [
            vec!["poddump", "--build-info"],
            vec!["poddump", "--pod-uid", uid, "--output", "/evidence/a"],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--max-write-mbps",
                "100",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--request-id",
                "req1",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--wait-timeout-ms",
                "100",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--format",
                "json",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--full-ids",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--jobs",
                "2",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--stream",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--pod",
                uid,
                "--output",
                "/evidence/a",
            ],
            vec![
                "poddump",
                "--pod",
                uid,
                "--output",
                "/evidence/a",
                "--output",
                "/evidence/b",
            ],
            vec!["poddump", "--pod", uid, "--output", "--help"],
            vec!["poddump", "--pod=", "--output", "/evidence/a"],
            vec!["poddump", "--pod", uid],
            vec!["poddump", "--output", "/evidence/a"],
        ];
        for case in rejected {
            assert!(parse_cli_args(&s(&case)).is_err(), "expected err: {case:?}");
        }
    }
}
