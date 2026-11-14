//! Per-Pod admission, held until final publication. OFD locks disappear on exit,
//! including SIGKILL. Never unlink a lock file: waiters must keep one inode.
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    time::{Duration, Instant},
};

pub struct Lease {
    _file: File,
    pub diagnostics: Value,
}
pub enum Admission {
    Acquired(Lease),
    Linked(Value),
}

const STALE_LOCK_MAX_AGE_SECS: u64 = 3600;

fn lock_range(file: &File, byte: i64, len: i64, kind: i16) -> Result<bool, String> {
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    range.l_type = kind;
    range.l_whence = libc::SEEK_SET as i16;
    range.l_start = byte;
    range.l_len = len;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &range) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if matches!(
        e.raw_os_error(),
        Some(libc::EAGAIN) | Some(libc::EACCES) | Some(libc::EINTR)
    ) {
        Ok(false)
    } else {
        Err(format!("Pod admission OFD lock: {e}"))
    }
}

fn lock(file: &File, byte: i64, kind: i16) -> Result<bool, String> {
    lock_range(file, byte, 1, kind)
}

/// Remove abandoned `pod-*.lock` files from prior completed runs when safe.
/// Requires an exclusive whole-file OFD lock so active holders/waiters are never unlinked.
fn gc_stale_locks(directory: &Path, active_path: &Path, current_boot: &str, max_age_secs: u64) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let now_wall = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == active_path {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("pod-") && name.ends_with(".lock")) {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
        else {
            continue;
        };
        if validate(&file, &path).is_err() {
            continue;
        }
        if !matches!(lock_range(&file, 0, 0, libc::F_WRLCK as i16), Ok(true)) {
            continue;
        }
        let stale_by_age = file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now_wall.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() >= max_age_secs);
        let stale_by_boot = !current_boot.is_empty() && {
            let mut f = &file;
            let _ = f.seek(SeekFrom::Start(0));
            let mut bytes = Vec::new();
            let _ = f.take(16384).read_to_end(&mut bytes);
            serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["boot_id"].as_str().map(str::to_string))
                .is_some_and(|b| !b.is_empty() && b != current_boot)
        };
        if stale_by_age || stale_by_boot {
            let _ = fs::remove_file(&path);
        }
        let _ = lock_range(&file, 0, 0, libc::F_UNLCK as i16);
    }
}
fn validate(file: &File, path: &Path) -> Result<(), String> {
    let a = file.metadata().map_err(|e| e.to_string())?;
    let b = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !a.is_file()
        || !b.is_file()
        || a.dev() != b.dev()
        || a.ino() != b.ino()
        || a.uid() != unsafe { libc::geteuid() }
        || a.mode() & 0o077 != 0
        || a.nlink() != 1
    {
        return Err(
            "Pod admission requires an unchanged private single-link file owned by caller".into(),
        );
    }
    Ok(())
}
pub fn acquire(
    directory: &Path,
    uid: &str,
    request_id: &str,
    output: &Path,
    timeout_ms: u64,
    cancelled: impl Fn() -> bool,
) -> Result<Admission, String> {
    crate::identity::valid_uid(uid)?;
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let path = directory.join(format!("pod-{uid}.lock"));
    gc_stale_locks(directory, &path, &boot_id, STALE_LOCK_MAX_AGE_SECS);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let begin = crate::now();
    let output = if output.is_absolute() {
        output.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(output)
    };
    loop {
        if cancelled() {
            return Err("cancelled while waiting for Pod admission; no target attached".into());
        }
        validate(&file, &path)?;
        // Byte 1 protects the short metadata transaction. Byte 0 owns the Pod.
        // Even a new holder cannot expose the preceding request's metadata.
        if lock(&file, 1, libc::F_WRLCK as i16)? {
            if lock(&file, 0, libc::F_WRLCK as i16)? {
                let info = json!({"pod_uid":uid,"request_id":request_id,"output":output,
                    "owner_pid":std::process::id(),"boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e|e.to_string())?.trim(),
                    "admitted_ns":crate::now(),"status":"in_progress"});
                file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
                let bytes = serde_json::to_vec(&info).map_err(|e| e.to_string())?;
                if bytes.len() > 16384 {
                    return Err("Pod admission metadata exceeds limit".into());
                }
                file.write_all(&bytes).map_err(|e| e.to_string())?;
                file.set_len(bytes.len() as u64)
                    .map_err(|e| e.to_string())?;
                lock(&file, 1, libc::F_UNLCK as i16)?;
                return Ok(Admission::Acquired(Lease {
                    _file: file,
                    diagnostics: json!({
                    "policy":"one active collection per Pod; identical active request-id links, other requests wait then rediscover",
                    "lock":path,"begin_ns":begin,"admitted_ns":crate::now(),"wait_ms":start.elapsed().as_millis(),
                    "wait_timeout_ms":timeout_ms,"request":info}),
                }));
            }
            file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(16385)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 16384 {
                return Err("Pod admission metadata exceeds limit".into());
            }
            let info: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid active Pod metadata: {e}"))?;
            lock(&file, 1, libc::F_UNLCK as i16)?;
            if info["pod_uid"] == uid && info["request_id"] == request_id {
                return Ok(Admission::Linked(
                    json!({"status":"already_running","active_request":info,
                    "requested_output_created":false,"capture_complete":false}),
                ));
            }
        }
        if start.elapsed() >= Duration::from_millis(timeout_ms) {
            return Err(format!(
                "Pod busy after {timeout_ms} ms; no target attached; retry with a new collection time"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_duplicate_links_distinct_request_waits_and_release_recovers() {
        let d = std::env::temp_dir().join(format!(
            "poddump_admission_{}_{}",
            std::process::id(),
            crate::now()
        ));
        fs::create_dir(&d).unwrap();
        let uid = "12345678-1234-1234-1234-123456789abc";
        let first = acquire(&d, uid, "a", Path::new("a"), 100, || false).unwrap();
        assert!(matches!(
            acquire(&d, uid, "a", Path::new("b"), 100, || false).unwrap(),
            Admission::Linked(_)
        ));
        assert!(
            acquire(&d, uid, "b", Path::new("b"), 1, || false)
                .unwrap_err_string()
                .contains("Pod busy")
        );
        assert!(acquire(&d, uid, "b", Path::new("b"), 100, || true).is_err());
        drop(first);
        assert!(matches!(
            acquire(&d, uid, "b", Path::new("b"), 100, || false).unwrap(),
            Admission::Acquired(_)
        ));
        fs::remove_file(d.join(format!("pod-{uid}.lock"))).unwrap();
        fs::remove_dir(d).unwrap();
    }

    #[test]
    fn stale_lock_gc_removes_dead_prior_boot_and_preserves_active_lock() {
        let d = std::env::temp_dir().join(format!(
            "poddump_lock_gc_{}_{}",
            std::process::id(),
            crate::now()
        ));
        fs::create_dir(&d).unwrap();
        let active_uid = "11111111-1111-1111-1111-111111111111";
        let stale_uid = "22222222-2222-2222-2222-222222222222";
        let active = acquire(&d, active_uid, "req-1", Path::new("out1"), 100, || false).unwrap();
        let stale_path = d.join(format!("pod-{stale_uid}.lock"));
        let mut stale_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&stale_path)
            .unwrap();
        stale_file
            .write_all(
                br#"{"pod_uid":"22222222-2222-2222-2222-222222222222","boot_id":"old-boot"}"#,
            )
            .unwrap();
        drop(stale_file);

        let exclude = d.join("pod-33333333-3333-3333-3333-333333333333.lock");
        gc_stale_locks(&d, &exclude, "current-boot", STALE_LOCK_MAX_AGE_SECS);
        assert!(d.join(format!("pod-{active_uid}.lock")).exists());
        assert!(!stale_path.exists());
        drop(active);
        fs::remove_file(d.join(format!("pod-{active_uid}.lock"))).unwrap();
        fs::remove_dir(d).unwrap();
    }
    trait ErrorText {
        fn unwrap_err_string(self) -> String;
    }
    impl ErrorText for Result<Admission, String> {
        fn unwrap_err_string(self) -> String {
            match self {
                Err(e) => e,
                Ok(_) => panic!("unexpected admission"),
            }
        }
    }
}
