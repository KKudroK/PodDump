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
    pub raw_path: Vec<u8>,
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

pub(crate) fn estimate(id: &crate::identity::Identity, uid: &str) -> Result<usize, String> {
    id.verify(uid)?;
    let process = procfs::read_process(id.pid).map_err(|e| e.to_string())?;
    let payload = process
        .mappings
        .iter()
        .filter(|m| m.should_dump)
        .try_fold(0usize, |n, m| n.checked_add((m.end - m.start) as usize))
        .ok_or("estimate overflow")?;
    let metadata = process
        .mappings
        .iter()
        .filter(|m| m.should_dump)
        .try_fold(1024 * 1024usize + elf::HEADER_RESERVE_BYTES, |n, m| {
            n.checked_add(m.path.len() + 128)
        })
        .ok_or("metadata overflow")?
        .checked_add(
            process
                .tids
                .len()
                .checked_mul(65536)
                .ok_or("thread estimate overflow")?,
        )
        .ok_or("metadata overflow")?;
    id.verify(uid)?;
    payload
        .checked_add(metadata)
        .ok_or_else(|| "estimate overflow".into())
}
pub(crate) struct Captured {
    pub details: serde_json::Value,
    output: File,
    snapshot: Option<elf::Snapshot>,
}

pub(crate) fn capture(
    id: &crate::identity::Identity,
    uid: &str,
    path: &Path,
    mut output: File,
    cancellation: &crate::engine::CancellationToken,
    limit: usize,
    stream: bool,
    reader: &str,
    output_limit: usize,
) -> Result<Captured, CorexError> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let pid = id.pid;
    let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    let pidfd: Option<OwnedFd> = if raw_fd >= 0 {
        Some(unsafe { OwnedFd::from_raw_fd(raw_fd) })
    } else {
        let err = std::io::Error::last_os_error();
        let code = err.raw_os_error().unwrap_or(0);
        if code == libc::ENOSYS || code == libc::EPERM {
            None
        } else {
            return Err(CorexError::io("pidfd_open", pid.to_string(), err));
        }
    };
    let check_alive = || {
        if let Some(ref fd) = pidfd {
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
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
        } else {
            let rc = unsafe { libc::kill(pid, 0) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    return Err(CorexError::InvalidData(
                        "process has exited (kill 0 returned ESRCH)".into(),
                    ));
                }
            }
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return Err(CorexError::InvalidData(
                    "process procfs directory missing".into(),
                ));
            }
            Ok(())
        }
    };
    id.verify(uid).map_err(CorexError::InvalidData)?;
    check_alive()?;
    // Allocate payload storage and bounded streaming scratch before stopping.
    let allocation_begin_ns = crate::now();
    let mut storage = if stream {
        None
    } else {
        Some(elf::Snapshot::allocate(limit)?)
    };
    let mut scratch = if stream {
        vec![0u8; 1024 * 1024]
    } else {
        Vec::new()
    };
    let mut stream_stats = if stream {
        Some(elf::IoStats::reserved()?)
    } else {
        None
    };
    let usage_begin = crate::usage();
    let allocation_end_ns = crate::now();
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
        let procfs_begin_ns = crate::now();
        let mut process = procfs::read_process(pid)?;
        let procfs_end_ns = crate::now();
        process.tids = attached.tids().to_vec();
        let mut ranges = Vec::new();
        for m in &mut process.mappings {
            ranges.push(serde_json::json!({
                "start": m.start,
                "end": m.end,
                "flags": m.flags,
                "path": m.path,
                "selected": m.should_dump,
                "is_shared": m.is_shared,
                "is_file_backed": m.is_file_backed,
                "reason": if m.should_dump {
                    "attempt_all_mapped_proc_mem"
                } else if procfs::is_kernel_special_unreadable(&m.path) {
                    "kernel_special_unreadable"
                } else {
                    "uncommitted_anonymous_prot_none"
                },
                "actual_reader": if m.should_dump {
                    "pinned_proc_mem"
                } else if procfs::is_kernel_special_unreadable(&m.path) {
                    "skipped_kernel_special"
                } else {
                    "skipped_uncommitted"
                }
            }));
        }
        let registers_begin_ns = crate::now();
        let threads = ptrace::read_thread_states(attached.tids(), Some(cancellation))?;
        let registers_end_ns = crate::now();
        let mut thread_identities = Vec::new();
        for tid in attached.tids() {
            let stat_path = format!("/proc/{pid}/task/{tid}/stat");
            let stat_raw = std::fs::read(&stat_path)
                .map_err(|e| CorexError::io("read thread identity", stat_path, e))?;
            let rparen = stat_raw
                .iter()
                .rposition(|&b| b == b')')
                .ok_or_else(|| CorexError::InvalidData("invalid thread stat: missing closing parenthesis".into()))?;
            let tail_bytes = stat_raw
                .get(rparen + 1..)
                .ok_or_else(|| CorexError::InvalidData("invalid thread stat: missing tail".into()))?;
            let tail = std::str::from_utf8(tail_bytes)
                .map_err(|_| CorexError::InvalidData("invalid thread stat encoding".into()))?;
            let starttime = tail
                .split_whitespace()
                .nth(19)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| CorexError::InvalidData("invalid thread starttime".into()))?;
            thread_identities.push(serde_json::json!({"tid":tid,"starttime":starttime}));
        }
        let notes_begin_ns = crate::now();
        let notes = notes::build(&process, &threads)?;
        let notes_end_ns = crate::now();
        let bytes: u64 = process
            .mappings
            .iter()
            .filter(|m| m.should_dump)
            .map(|m| m.end - m.start)
            .sum();
        let required = bytes
            .checked_add(notes.len() as u64 + elf::HEADER_RESERVE_BYTES as u64)
            .ok_or_else(|| CorexError::InvalidData("snapshot size overflow".into()))?;
        if required > output_limit as u64 || (!stream && required > limit as u64) {
            return Err(CorexError::InvalidData(
                "mapping growth exceeds pre-stop reservation; detach and report failure".into(),
            ));
        }
        let buffered = !stream;
        let mut snapshot = None;
        let memory_read_begin_ns = crate::now();
        let mut io_stats = serde_json::Value::Null;
        let captured = (|| {
            if buffered {
                snapshot = Some(elf::snapshot_reserved(
                    &process,
                    &notes,
                    Some(cancellation),
                    &memory,
                    reader,
                    &check_alive,
                    storage.take(),
                )?);
                io_stats = snapshot.as_ref().unwrap().stats.json();
            } else {
                io_stats = elf::write_reserved(
                    path,
                    elf::SparseWriter { file: &mut output },
                    &process,
                    &notes,
                    Some(cancellation),
                    &memory,
                    &mut scratch,
                    stream_stats.take(),
                )?
                .json();
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
            serde_json::json!({"memory_io":io_stats,"procfs_begin_ns":procfs_begin_ns,"procfs_end_ns":procfs_end_ns,"registers_begin_ns":registers_begin_ns,"registers_end_ns":registers_end_ns,"notes_begin_ns":notes_begin_ns,"notes_end_ns":notes_end_ns,"memory_read_begin_ns":memory_read_begin_ns}),
        ))
    })();
    let captured_ns = crate::now();
    let detached = attached.finish();
    let resumed_ns = crate::now();
    let (process, ranges, bytes, buffered, snapshot, thread_identities, memory_pinned_ns, stages) =
        match (prepared, detached) {
            (Err(e), Err(d)) => return Err(e.with_cleanup(d)),
            (Err(e), _) | (_, Err(e)) => return Err(e),
            (Ok(v), Ok(())) => v,
        };
    Ok(Captured {
        details: serde_json::json!({"allocation_begin_ns":allocation_begin_ns,"allocation_end_ns":allocation_end_ns,"stages":stages,"capture_thread_usage_begin":usage_begin,"capture_thread_usage_end":crate::usage(),"ranges":ranges,"tids":process.tids,"thread_identities":thread_identities,"requested_memory_bytes":bytes,"payload_bytes":stages["memory_io"]["coverage"]["captured_bytes"],"buffered":buffered,"actual_reader":if buffered{reader}else{"proc_mem"},"attach_begin_ns":attach_begin,"all_threads_stopped_ns":stopped,"memory_pinned_ns":memory_pinned_ns,"capture_complete_ns":captured_ns,"detach_complete_ns":resumed_ns,"write_complete_ns":null,"writeback":null,"durable_ns":null,"deferred_durability":true,"census_state":id.state,"observed_coredump_filter":process.coredump_filter,"executable":process.exe,"preserved_group_stop":preserved_group_stop,"coverage":stages["memory_io"]["coverage"],"read_errors":stages["memory_io"]["coverage"]["read_errors"],"reserved_output_bytes":output_limit,"reserved_snapshot_bytes":if buffered{limit}else{0},"stream_scratch_bytes":if buffered{0}else{1024*1024}}),
        output,
        snapshot,
    })
}

impl Captured {
    pub(crate) fn write(
        mut self,
        path: &Path,
        cancellation: &crate::engine::CancellationToken,
        hint: bool,
    ) -> Result<(serde_json::Value, File), CorexError> {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::fd::AsRawFd;
        let mut write_calls = 0u64;
        let mut written_bytes = 0usize;
        let mut short_writes = 0u64;
        let mut sparse_bytes = 0usize;
        let mut zero_scan_ns = 0u128;
        let mut write_call_ns = 0u128;
        let detailed = crate::TRACE.load(crate::Ordering::Relaxed);
        if cancellation.is_cancelled() {
            return Err(if cancellation.timed_out() {
                CorexError::TimedOut
            } else {
                CorexError::Cancelled
            });
        }
        self.details["writer_thread_usage_begin"] = crate::usage();
        self.details["writer_begin_ns"] = serde_json::json!(crate::now());
        if let Some(snapshot) = self.snapshot.take() {
            const SPARSE_BLOCK_BYTES: usize = 64 * 1024;
            const MAX_WRITE_BATCH_BYTES: usize = 4 * 1024 * 1024;
            let bytes = snapshot.bytes();
            let total_len = bytes.len();
            let mut offset = 0usize;
            let mut cached_block: Option<(usize, bool)> = None;
            let is_block_zero = |start: usize,
                                 end: usize,
                                 cache: &mut Option<(usize, bool)>,
                                 scan_ns: &mut u128|
             -> bool {
                if let Some((cached_start, val)) = cache.take() {
                    if cached_start == start {
                        return val;
                    }
                }
                let sb = if detailed { crate::now() } else { 0 };
                let z = elf::is_all_zero(&bytes[start..end]);
                if detailed {
                    *scan_ns += crate::now() - sb;
                }
                z
            };
            while offset < total_len {
                if cancellation.is_cancelled() {
                    return Err(if cancellation.timed_out() {
                        CorexError::TimedOut
                    } else {
                        CorexError::Cancelled
                    });
                }
                let block_end = (offset + SPARSE_BLOCK_BYTES).min(total_len);
                if is_block_zero(offset, block_end, &mut cached_block, &mut zero_scan_ns) {
                    let mut hole_end = block_end;
                    while hole_end < total_len {
                        let next_end = (hole_end + SPARSE_BLOCK_BYTES).min(total_len);
                        if !is_block_zero(hole_end, next_end, &mut cached_block, &mut zero_scan_ns)
                        {
                            cached_block = Some((hole_end, false));
                            break;
                        }
                        hole_end = next_end;
                    }
                    let hole_len = hole_end - offset;
                    sparse_bytes += hole_len;
                    self.output
                        .seek(SeekFrom::Current(hole_len as i64))
                        .map_err(|e| {
                            CorexError::io("seek sparse output", path.display().to_string(), e)
                        })?;
                    offset = hole_end;
                } else {
                    let mut data_end = block_end;
                    while data_end < total_len
                        && (data_end - offset) + SPARSE_BLOCK_BYTES <= MAX_WRITE_BATCH_BYTES
                    {
                        let next_end = (data_end + SPARSE_BLOCK_BYTES).min(total_len);
                        if is_block_zero(data_end, next_end, &mut cached_block, &mut zero_scan_ns) {
                            cached_block = Some((data_end, true));
                            break;
                        }
                        data_end = next_end;
                    }
                    let mut remaining = &bytes[offset..data_end];
                    while !remaining.is_empty() {
                        let begin = if detailed { crate::now() } else { 0 };
                        let written = self.output.write(remaining);
                        write_calls += 1;
                        if detailed {
                            write_call_ns += crate::now() - begin;
                        }
                        match written {
                            Ok(0) => {
                                return Err(CorexError::InvalidData(
                                    "zero-length snapshot write".into(),
                                ));
                            }
                            Ok(n) => {
                                short_writes += u64::from(n < remaining.len());
                                written_bytes += n;
                                remaining = &remaining[n..];
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                return Err(CorexError::io(
                                    "write snapshot",
                                    path.display().to_string(),
                                    e,
                                ));
                            }
                        }
                    }
                    offset = data_end;
                }
            }
            self.output
                .set_len(total_len as u64)
                .map_err(|e| CorexError::io("truncate output", path.display().to_string(), e))?;
        }
        self.details["writer_io"] = serde_json::json!({"write_calls":write_calls,"written_bytes":written_bytes,"short_writes":short_writes,"sparse_bytes":sparse_bytes,"zero_scan_ns":if detailed{Some(zero_scan_ns)}else{None},"write_call_ns":if detailed{Some(write_call_ns)}else{None}});
        self.details["writer_thread_usage_end"] = crate::usage();
        if cancellation.is_cancelled() {
            return Err(if cancellation.timed_out() {
                CorexError::TimedOut
            } else {
                CorexError::Cancelled
            });
        }
        self.details["write_complete_ns"] = serde_json::json!(crate::now());
        if hint {
            let begin = crate::now();
            let rc = unsafe {
                libc::sync_file_range(self.output.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE)
            };
            if rc != 0 {
                return Err(CorexError::io(
                    "request writeback",
                    path.display().to_string(),
                    std::io::Error::last_os_error(),
                ));
            }
            self.details["writeback"] =
                serde_json::json!({"begin_ns":begin,"end_ns":crate::now(),"durable":false});
        }
        Ok((self.details, self.output))
    }
}

#[derive(Debug)]
pub(crate) enum CorexError {
    Io {
        operation: &'static str,
        path: String,
        source: std::io::Error,
    },
    CaptureIncomplete {
        cause: Box<CorexError>,
        coverage: Box<serde_json::Value>,
    },
    InvalidData(String),
    Ptrace(String),
    Cancelled,
    TimedOut,
}

impl CorexError {
    pub(crate) fn with_coverage(self, coverage: serde_json::Value) -> Self {
        Self::CaptureIncomplete {
            cause: Box::new(self),
            coverage: Box::new(coverage),
        }
    }
    pub(crate) fn coverage_details(&self) -> Option<serde_json::Value> {
        match self {
            Self::CaptureIncomplete { coverage, .. } => {
                Some(serde_json::json!({"coverage":coverage,"read_errors":coverage["read_errors"]}))
            }
            _ => None,
        }
    }
    fn with_cleanup(self, cleanup: CorexError) -> Self {
        let cause = Self::InvalidData(format!("{self}; cleanup: {cleanup}"));
        match self {
            Self::CaptureIncomplete { coverage, .. } => cause.with_coverage(*coverage),
            _ => cause,
        }
    }

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
            Self::CaptureIncomplete { cause, .. } => write!(formatter, "{cause}"),
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
            Self::CaptureIncomplete { cause, .. } => Some(cause.as_ref()),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod deadline_regressions {
    use super::*;
    #[test]
    fn completed_stream_cannot_ignore_expired_writer_deadline() {
        let path = std::env::temp_dir().join(format!(
            "poddump_stream_deadline_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let file = File::create(&path).unwrap();
        let captured = Captured {
            output: file,
            snapshot: None,
            details: serde_json::json!({}),
        };
        let token = crate::engine::CancellationToken::with_timeout(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(matches!(
            captured.write(&path, &token, false),
            Err(CorexError::TimedOut)
        ));
        std::fs::remove_file(path).unwrap();
    }
}
