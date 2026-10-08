mod elf;
mod notes;
mod procfs;
mod ptrace;

use std::fmt;
use std::fs::File;
use std::path::Path;

#[derive(Clone, Debug)]
pub(super) struct Mapping {
    pub start: u64,
    pub end: u64,
    pub flags: u32,
    pub offset: u64,
    pub is_shared: bool,
    pub is_file_backed: bool,
    pub should_dump: bool,
    pub path: String,
}

#[derive(Clone, Debug)]
pub(super) struct ProcessInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgrp: i32,
    pub sid: i32,
    pub uid: u32,
    pub gid: u32,
    pub comm: Vec<u8>,
    pub exe: String,
    pub cmdline: Vec<u8>,
    pub mappings: Vec<Mapping>,
    pub auxv: Vec<u8>,
    pub coredump_filter: u32,
    pub page_size: u64,
    pub tids: Vec<i32>,
}

#[derive(Clone, Debug)]
pub(super) struct ThreadState {
    pub tid: i32,
    pub gp_regs: Vec<u8>,
    pub fp_regs: Vec<u8>,
    pub pac_mask: Option<[u64; 2]>,
    pub xstate: Vec<u8>,
}

pub(crate) fn dump(
    id: &crate::identity::Identity,
    uid: &str,
    path: &Path,
    mut output: File,
    cancellation: &crate::engine::CancellationToken,
    scope: &str,
    limit: usize,
    stream: bool,
    reader: &str,
    defer_durability: bool,
) -> Result<(serde_json::Value, File), CorexError> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let pid = id.pid;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        return Err(CorexError::io(
            "pidfd_open",
            pid.to_string(),
            std::io::Error::last_os_error(),
        ));
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
    let check_alive = || {
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut poll, 1, 0) };
        if rc != 0 {
            Err(CorexError::InvalidData(
                "pidfd indicates exit or poll error".into(),
            ))
        } else {
            Ok(())
        }
    };
    id.verify(uid).map_err(CorexError::InvalidData)?;
    check_alive()?;
    let attach_begin = crate::now();
    let attached = ptrace::AttachedThreads::attach_process(pid, Some(cancellation))?;
    let preserved_group_stop = attached.preserved_group_stop();
    let stopped = crate::now();
    let prepared = (|| {
        check_alive()?;
        id.verify(uid).map_err(CorexError::InvalidData)?;
        // An open proc_mem FD pins an mm, not an exec epoch. Pin only after all
        // threads are stopped, so a pre-attach exec cannot leave us reading an
        // older mm retained by a different CLONE_VM thread group.
        let memory_path = format!("/proc/{pid}/mem");
        let memory = File::open(&memory_path)
            .map_err(|e| CorexError::io("open pinned memory", &memory_path, e))?;
        check_alive()?;
        id.verify(uid).map_err(CorexError::InvalidData)?;
        let memory_pinned_ns = crate::now();
        let mut process = procfs::read_process(pid)?;
        process.tids = attached.tids().to_vec();
        process.coredump_filter = 0x32;
        if scope == "legacy32" {
            procfs::apply_coredump_filter(&mut process, &memory)?;
        }
        let mut ranges = Vec::new();
        for m in &mut process.mappings {
            let reason = if matches!(m.path.as_str(), "[vvar]" | "[vvar_vclock]" | "[vsyscall]") {
                m.should_dump = false;
                "kernel_special_unreadable"
            } else if scope == "readable" {
                m.should_dump = m.flags & 4 != 0;
                if m.should_dump {
                    "selected_readable"
                } else {
                    "not_readable"
                }
            } else {
                if m.should_dump {
                    "selected_legacy32"
                } else {
                    "legacy32_excluded"
                }
            };
            ranges.push(serde_json::json!({"start":m.start,"end":m.end,"flags":m.flags,"path":m.path,"selected":m.should_dump,"reason":reason}));
        }
        let threads = ptrace::read_thread_states(attached.tids(), Some(cancellation))?;
        let mut thread_identities = Vec::new();
        for tid in attached.tids() {
            let stat_path = format!("/proc/{pid}/task/{tid}/stat");
            let stat = std::fs::read_to_string(&stat_path)
                .map_err(|e| CorexError::io("read thread identity", stat_path, e))?;
            let tail = stat
                .rsplit_once(')')
                .ok_or_else(|| CorexError::InvalidData("invalid thread stat".into()))?
                .1;
            let starttime = tail
                .split_whitespace()
                .nth(19)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| CorexError::InvalidData("invalid thread starttime".into()))?;
            thread_identities.push(serde_json::json!({"tid":tid,"starttime":starttime}));
        }
        let notes = notes::build(&process, &threads)?;
        let bytes: u64 = process
            .mappings
            .iter()
            .filter(|m| m.should_dump)
            .map(|m| m.end - m.start)
            .sum();
        let buffered = !stream
            && bytes.saturating_add(
                notes.len() as u64 + (process.mappings.len() as u64 + 1) * 56 + 8192,
            ) <= limit as u64;
        for range in &mut ranges {
            if range["selected"] == true {
                range["actual_reader"] = serde_json::json!(if buffered
                    && reader == "readv"
                    && range["flags"].as_u64().unwrap() & 4 != 0
                {
                    "process_vm_readv"
                } else {
                    "pinned_proc_mem"
                });
            }
        }
        let mut snapshot = None;
        let captured = (|| {
            if buffered {
                snapshot = Some(elf::snapshot(
                    &process,
                    &notes,
                    Some(cancellation),
                    &memory,
                    reader,
                    &check_alive,
                )?);
            } else {
                elf::write(
                    path,
                    elf::SparseWriter { file: &mut output },
                    &process,
                    &notes,
                    Some(cancellation),
                    &memory,
                )?;
            }
            check_alive()?;
            id.verify(uid).map_err(CorexError::InvalidData)
        })();
        captured?;
        Ok((
            process,
            ranges,
            bytes,
            buffered,
            snapshot,
            thread_identities,
            memory_pinned_ns,
        ))
    })();
    let captured_ns = crate::now();
    let detached = attached.finish();
    let resumed_ns = crate::now();
    let (process, ranges, bytes, buffered, snapshot, thread_identities, memory_pinned_ns) =
        match (prepared, detached) {
            (Err(e), Err(d)) => return Err(CorexError::InvalidData(format!("{e}; cleanup: {d}"))),
            (Err(e), _) | (_, Err(e)) => return Err(e),
            (Ok(v), Ok(())) => v,
        };
    if buffered {
        let snapshot = snapshot.as_ref().expect("buffered snapshot").bytes();
        for chunk in snapshot.chunks(1024 * 1024) {
            if cancellation.is_cancelled() {
                return Err(if cancellation.timed_out() {
                    CorexError::TimedOut
                } else {
                    CorexError::Cancelled
                });
            }
            if chunk.iter().all(|b| *b == 0) {
                output
                    .seek(SeekFrom::Current(chunk.len() as i64))
                    .map_err(|e| {
                        CorexError::io("seek sparse output", path.display().to_string(), e)
                    })?;
            } else {
                output
                    .write_all(chunk)
                    .map_err(|e| CorexError::io("write snapshot", path.display().to_string(), e))?;
            }
        }
        output
            .set_len(snapshot.len() as u64)
            .map_err(|e| CorexError::io("truncate output", path.display().to_string(), e))?;
    }
    let write_complete_ns = crate::now();
    // Start writeback without claiming durability. Retain this exact open file
    // description until fsync so delayed writeback errors cannot be lost by a
    // close/reopen between writing and the final barrier.
    let writeback = if defer_durability {
        let begin = crate::now();
        let rc = unsafe { libc::sync_file_range(output.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE) };
        if rc == 0 {
            serde_json::json!({"requested":true,"begin_ns":begin,"end_ns":crate::now(),"durable":false})
        } else {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)) {
                serde_json::json!({"requested":false,"fallback":"final_fsync","error":error.to_string(),"begin_ns":begin,"end_ns":crate::now(),"durable":false})
            } else {
                return Err(CorexError::io("request writeback", path.display().to_string(), error));
            }
        }
    } else { serde_json::Value::Null };
    let durable_ns = if defer_durability {
        None
    } else {
        output.sync_all()
            .map_err(|e| CorexError::io("fsync output", path.display().to_string(), e))?;
        Some(crate::now())
    };
    Ok((
        serde_json::json!({"ranges":ranges,"tids":process.tids,"thread_identities":thread_identities,"payload_bytes":bytes,"buffered":buffered,"actual_reader":if buffered{reader}else{"proc_mem"},"attach_begin_ns":attach_begin,"all_threads_stopped_ns":stopped,"memory_pinned_ns":memory_pinned_ns,"capture_complete_ns":captured_ns,"detach_complete_ns":resumed_ns,"write_complete_ns":write_complete_ns,"writeback":writeback,"durable_ns":durable_ns,"deferred_durability":defer_durability,"census_state":id.state,"preserved_group_stop":preserved_group_stop,"read_errors":[]}), output
    ))
}

#[derive(Debug)]
pub(crate) enum CorexError {
    Io {
        operation: &'static str,
        path: String,
        source: std::io::Error,
    },
    InvalidData(String),
    Ptrace(String),
    Cancelled,
    TimedOut,
}

impl CorexError {
    pub(super) fn io(
        operation: &'static str,
        path: impl Into<String>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for CorexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "failed to {operation} {path}: {source}"),
            Self::InvalidData(message) | Self::Ptrace(message) => formatter.write_str(message),
            Self::TimedOut => formatter.write_str("per-process capture deadline exceeded"),
            Self::Cancelled => formatter.write_str("core dump generation was cancelled"),
        }
    }
}

impl std::error::Error for CorexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
