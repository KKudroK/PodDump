mod corex;
mod cli_output;
mod identity;
use serde_json::{Value, json};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
static CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn signal(_: i32) {
    CANCEL.store(true, Ordering::Relaxed);
}
pub mod engine {
    #[derive(Default)]
    pub struct CancellationToken {
        deadline: Option<std::time::Instant>,
    }
    impl CancellationToken {
        pub fn with_timeout(ms: u64) -> Self {
            Self {
                deadline: Some(std::time::Instant::now() + std::time::Duration::from_millis(ms)),
            }
        }
        pub fn timed_out(&self) -> bool {
            self.deadline
                .is_some_and(|d| std::time::Instant::now() >= d)
        }
        pub fn is_cancelled(&self) -> bool {
            super::CANCEL.load(super::Ordering::Relaxed) || self.timed_out()
        }
        pub fn cancel(&self) {
            super::CANCEL.store(true, super::Ordering::Relaxed);
        }
    }
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
fn atomic_json(path: &Path, v: &Value) -> Result<(), String> {
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
    fs::rename(tmp, path).map_err(|e| format!("rename manifest: {e}"))?;
    let output_directory = path.parent().unwrap();
    File::open(output_directory)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("fsync output directory: {e}"))?;
    let parent = output_directory.parent()
        .filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    File::open(parent).and_then(|f| f.sync_all())
        .map_err(|e| format!("fsync output parent directory: {e}"))
}
fn finalize_core(out: &Path, row: &mut Value, owned_file: Option<File>) {
    if row["status"] != "captured_pending_durability" {
        return;
    }
    row["details"]["durability_begin_ns"] = json!(now());
    let partial = out.join(row["partial_file"].as_str().expect("generated partial name"));
    let final_path = partial.with_extension("");
    let result = (|| -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let file = owned_file.ok_or("missing original write file description")?;
        let original = file.metadata().map_err(|e| format!("stat original core: {e}"))?;
        let named = fs::symlink_metadata(&partial).map_err(|e| format!("stat partial core path: {e}"))?;
        if !named.file_type().is_file() || original.dev() != named.dev() || original.ino() != named.ino() {
            return Err("partial core path no longer names the original captured inode".into());
        }
        row["details"]["durability_fd"] = json!("original_write_description");
        if row["details"]["durable_ns"].is_null() {
            file.sync_all().map_err(|e| format!("fsync core: {e}"))?;
            row["details"]["durable_ns"] = json!(now());
        }
        fs::rename(&partial, &final_path).map_err(|e| format!("publish durable core: {e}"))?;
        Ok(())
    })();
    row["end_ns"] = json!(now());
    match result {
        Ok(()) => {
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
fn run() -> Result<bool, String> {
    let total_begin = now();
    let total_timer = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 || args.iter().any(|s| s == "--help") {
        println!(
            "PodDump {} (ProcDump Linux 3.5.3 corex-derived)\nUsage: poddump --pod UUID --output NEW_DIR [OPTIONS]\n\n  --pod UUID                     Full Pod UID on the local node\n  --output NEW_DIR               New output directory\n  --format tree|json             Terminal output (default: tree)\n  --full-ids                     Show full container IDs in the tree\n  --jobs 1..16                   Concurrent process workers (default: 4)\n  --scope readable|legacy32      Mapping policy (default: readable)\n  --reader proc_mem|readv        Memory reader (default: proc_mem)\n  --memory-mib N                 Snapshot budget (default: 512)\n  --buffer-mib N                 Per-worker buffer ceiling (default: 512)\n  --stream                      Capture while stopped\n  --durability batch|per-process Persistence scheduling (default: batch)\n  --census-rounds 0..8           Additional census rounds (default: 2)\n  --timeout-ms 1..3600000        Capture/write deadline (default: 60000)\n\nCompatibility: --pod-uid is an alias for --pod.\nLocal Linux cgroup v2/containerd host; includes sandbox processes.\nEach process is captured independently; this is not a Pod-atomic snapshot.", env!("CARGO_PKG_VERSION")
        );
        return Ok(true);
    }
    let valued = [
        "--pod",
        "--pod-uid",
        "--output",
        "--jobs",
        "--scope",
        "--buffer-mib",
        "--memory-mib",
        "--census-rounds",
        "--timeout-ms",
        "--reader",
        "--durability",
        "--format",
    ];
    let mut i = 1;
    let mut keys = std::collections::BTreeSet::new();
    while i < args.len() {
        let k = &args[i];
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
    let reader = val("--reader", "proc_mem");
    if reader != "readv" && reader != "proc_mem" {
        return Err("invalid reader".into());
    }
    let scope = val("--scope", "readable");
    if scope != "legacy32" && scope != "readable" {
        return Err("invalid scope".into());
    }
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
    let limit = limit.min(aggregate / jobs);
    let stream = args.iter().any(|s| s == "--stream");
    let durability = val("--durability", "batch");
    if durability != "batch" && durability != "per-process" { return Err("invalid durability".into()); }
    let defer_durability = durability == "batch";
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
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&out)
        .map_err(|e| format!("create new output: {e}"))?;
    let reserve = out.join("manifest_reserve");
    let reserve_file = OpenOptions::new()
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
        return Err(format!(
            "cannot reserve manifest space: {}",
            std::io::Error::from_raw_os_error(rc)
        ));
    }
    reserve_file.sync_all().map_err(|e| e.to_string())?;
    drop(reserve_file);
    let begin = total_begin;
    let timer = total_timer;
    let results = Mutex::new(Vec::<(Value, Option<File>)>::new());
    let mut descriptor_limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut descriptor_limit) } != 0 { return Err("cannot read descriptor limit".into()); }
    let pending_fd_limit = descriptor_limit.rlim_cur.saturating_sub(64).min(256) as usize;
    if pending_fd_limit == 0 { return Err("at least65 file descriptors are required".into()); }
    let mut durability_barriers = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut batch = initial.clone();
    let mut censuses = vec![json!({"stage":"initial","errors":initial_errors})];
    for round in 0..=rounds {
      for wave in batch.chunks(pending_fd_limit) {
        let wave_start = results.lock().unwrap().len();
        let idx = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..jobs {
                s.spawn(||{loop{
            let i=idx.fetch_add(1,Ordering::Relaxed);let Some(id)=wave.get(i) else{break};
            let started=now();let file=out.join(format!("{}_{}.core.partial",id.pid,id.starttime));
            let token=engine::CancellationToken::with_timeout(timeout);
            let result=(||{if token.is_cancelled(){return Err("cancelled before attach".into())} id.verify(&uid)?;
                let f=OpenOptions::new().create_new(true).write(true).mode(0o600).open(&file).map_err(|e|e.to_string())?;
                let details=corex::dump(id,&uid,&file,f,&token,&scope,limit,stream,&reader,defer_durability).map_err(|e|e.to_string())?;
                Ok::<_,String>(details)})();
            let row=match result{Ok((details,owned_file))=>(json!({"identity":id,"status":"captured_pending_durability","round":round,"begin_ns":started,"worker_complete_ns":now(),"partial_file":file.file_name().unwrap().to_string_lossy(),"details":details}),Some(owned_file)),Err(e)=>(json!({"identity":id,"status":"failed","round":round,"begin_ns":started,"end_ns":now(),"error":e,"partial_file":file.file_name().unwrap().to_string_lossy()}),None)};
            results.lock().unwrap().push(row);
        }});
            }
        });
        let barrier_begin = now();
        {
            let mut rows = results.lock().unwrap();
            let pending = &mut rows[wave_start..];
            let chunk_size = pending.len().div_ceil(jobs).max(1);
            std::thread::scope(|scope| {
                for chunk in pending.chunks_mut(chunk_size) {
                    scope.spawn(|| { for (row, file) in chunk { finalize_core(&out, row, file.take()); } });
                }
            });
        }
        durability_barriers.push(json!({"round":round,"processes":wave.len(),"begin_ns":barrier_begin,"end_ns":now()}));
      }
        for id in &batch {
            seen.insert((id.pid, id.starttime));
        }
        match identity::census(&root, &uid) {
            Ok((census, errors)) => {
                censuses.push(
                    json!({"round":round,"time_ns":now(),"processes":census,"errors":errors}),
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
    let results: Vec<Value> = results.into_inner().unwrap().into_iter().map(|(row, _)| row).collect();
    let durability_begin_ns = durability_barriers.first().map(|x| x["begin_ns"].clone());
    let durability_complete_ns = durability_barriers.last().map(|x| x["end_ns"].clone());
    let cancel_requested = CANCEL.load(Ordering::Relaxed);
    let success = !cancel_requested && results.iter().all(|x| x["status"] == "complete")
        && batch.is_empty()
        && !censuses.iter().any(|x| {
            x.get("error").is_some()
                || x.get("errors")
                    .and_then(|e| e.as_array())
                    .is_some_and(|e| !e.is_empty())
        });
    let manifest = json!({"schema":1,"durability_contract":"Manifest status describes collection completeness; successful process exit is also required to confirm final manifest/output/parent directory persistence. On persistence failure preserve stderr emergency_manifest and nonzero exit.","build_provenance":serde_json::from_str::<Value>(include_str!("../build_provenance.json")).unwrap(),"timebase":"CLOCK_MONOTONIC nanoseconds","wall_time_unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),"tool":"PodDump","version":env!("CARGO_PKG_VERSION"),"upstream":"ProcDump-Linux 3.5.3 8e34cbfaf57b38ca6b73160031e360edcbe6b9c1","pod_uid":uid,"cgroup_root":root,"boot_id":boot.trim(),"node":fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),"scope":scope,"durability":durability,"pending_fd_limit":pending_fd_limit,"durability_barriers":durability_barriers,"durability_begin_ns":durability_begin_ns,"durability_complete_ns":durability_complete_ns,"reader":reader,"initial_census_stable":initial_stable,"timeout_per_process_ms":timeout,"timeout_scope":"cooperative capture/write deadline; final durability attempts finish after capture; kernel I/O may block","cancel_requested":cancel_requested,"jobs":jobs,"buffer_limit_per_worker":limit,"stream":stream,"begin_ns":begin,"discovery_complete_ns":discovery_complete,"discovery_s":discovery_s,"memory_budget_bytes":aggregate,"end_ns":now(),"elapsed_s":timer.elapsed().as_secs_f64(),"initial":initial,"censuses":censuses,"uncaptured_new":batch,"processes":results,"status":if success{"complete"}else{"partial"},"atomicity":"Each process stopped independently; shared mappings may change externally; not a Pod-atomic snapshot","registers":"GP and FP regsets (upstream), x86 XSAVE additionally captured"});
    fs::remove_file(&reserve).map_err(|e| e.to_string())?;
    if let Err(e) = atomic_json(&out.join("manifest.json"), &manifest) {
        eprintln!(
            "{}",
            json!({"manifest_write_error":e,"emergency_manifest":manifest})
        );
        return Err(e);
    };
    let elapsed_s = timer.elapsed().as_secs_f64();
    if format == "json" {
        println!("{}", json!({"status":manifest["status"],"manifest":out.join("manifest.json"),"elapsed_s":elapsed_s}));
    } else {
        println!("{}", cli_output::tree(&manifest, &out, elapsed_s, full_ids));
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
    fn directory() -> PathBuf {
        let p = std::env::temp_dir().join(format!("poddump_durability_{}_{}",std::process::id(),now()));
        fs::create_dir(&p).unwrap();p
    }
    fn pending(name: &str) -> Value {
        json!({"status":"captured_pending_durability","partial_file":name,"details":{"durable_ns":null}})
    }
    #[test]
    fn pending_core_becomes_durable_before_publication() {
        let d=directory();let p=d.join("1.core.partial");fs::write(&p,b"immutable captured bytes").unwrap();
        let file=OpenOptions::new().write(true).open(&p).unwrap();
        let mut row=pending("1.core.partial");finalize_core(&d,&mut row,Some(file));
        assert_eq!(row["status"],"complete");assert!(row["details"]["durable_ns"].as_u64().unwrap()>0);
        assert_eq!(fs::read(d.join("1.core")).unwrap(),b"immutable captured bytes");assert!(!p.exists());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn symlink_replacement_is_failed_and_never_published() {
        use std::os::unix::fs::symlink;
        let d=directory();let target=d.join("unrelated");fs::write(&target,b"preserve").unwrap();
        let file=File::create(d.join("original")).unwrap();
        symlink(&target,d.join("1.core.partial")).unwrap();let mut row=pending("1.core.partial");
        finalize_core(&d,&mut row,Some(file));assert_eq!(row["status"],"failed");assert_eq!(row["failure_stage"],"durability");
        assert!(row["details"]["durable_ns"].is_null());assert!(!d.join("1.core").exists());
        assert_eq!(fs::read(target).unwrap(),b"preserve");fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn failed_capture_is_not_promoted_by_durability() {
        let d=directory();fs::write(d.join("1.core.partial"),b"incomplete").unwrap();
        let mut row=json!({"status":"failed","partial_file":"1.core.partial","error":"memory read failed"});
        let original=row.clone();finalize_core(&d,&mut row,None);assert_eq!(row,original);assert!(!d.join("1.core").exists());
        fs::remove_dir_all(d).unwrap();
    }
}
