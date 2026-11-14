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
    pub exclusion_reason: Option<&'static str>,
    pub vm_flags_disallow_pagemap_skip: bool,
    pub rss_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
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
    let dumped_count = process.mappings.iter().filter(|m| m.should_dump).count();
    let metadata = process
        .mappings
        .iter()
        .filter(|m| m.should_dump)
        .try_fold(
            1024 * 1024usize + elf::header_reserve_bytes_for(dumped_count),
            |n, m| n.checked_add(m.path.len() + 128),
        )
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
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaptureOptions {
    pub allow_pagemap_skip: bool,
    pub max_write_mbps: Option<u64>,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            allow_pagemap_skip: true,
            max_write_mbps: None,
        }
    }
}

pub(crate) struct Captured {
    pub details: serde_json::Value,
    output: File,
    snapshot: Option<elf::Snapshot>,
    max_write_mbps: Option<u64>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture(
    id: &crate::identity::Identity,
    uid: &str,
    path: &Path,
    output: File,
    cancellation: &crate::engine::CancellationToken,
    limit: usize,
    stream: bool,
    reader: &str,
    output_limit: usize,
) -> Result<Captured, CorexError> {
    capture_with_options(
        id,
        uid,
        path,
        output,
        cancellation,
        limit,
        stream,
        reader,
        output_limit,
        CaptureOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_with_options(
    id: &crate::identity::Identity,
    uid: &str,
    path: &Path,
    mut output: File,
    cancellation: &crate::engine::CancellationToken,
    limit: usize,
    stream: bool,
    reader: &str,
    output_limit: usize,
    options: CaptureOptions,
) -> Result<Captured, CorexError> {
    let result = (|| -> Result<(serde_json::Value, Option<elf::Snapshot>), CorexError> {
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
            Some(elf::IoStats::reserved_for(128)?)
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
            let pagemap = if options.allow_pagemap_skip {
                File::open(format!("/proc/{pid}/pagemap")).ok()
            } else {
                None
            };
            check_alive()?;
            id.verify(uid).map_err(CorexError::InvalidData)?;
            let memory_pinned_ns = crate::now();
            let procfs_begin_ns = crate::now();
            let mut process = procfs::read_process(pid)?;
            let procfs_end_ns = crate::now();
            process.tids = attached.tids().to_vec();
            let mut ranges = Vec::new();
            for m in &mut process.mappings {
                let (reason, actual_reader) = if m.should_dump {
                    ("attempt_all_mapped_proc_mem", "pinned_proc_mem")
                } else {
                    match m.exclusion_reason {
                        Some("kernel_special_unreadable") => {
                            ("kernel_special_unreadable", "skipped_kernel_special")
                        }
                        Some("device_io_or_pfnmap") => ("device_io_or_pfnmap", "skipped_device_io"),
                        _ => ("uncommitted_anonymous_prot_none", "skipped_uncommitted"),
                    }
                };
                ranges.push(serde_json::json!({
                    "start": m.start,
                    "end": m.end,
                    "flags": m.flags,
                    "path": m.path,
                    "selected": m.should_dump,
                    "is_shared": m.is_shared,
                    "is_file_backed": m.is_file_backed,
                    "vm_flags_disallow_pagemap_skip": m.vm_flags_disallow_pagemap_skip,
                    "rss_bytes": m.rss_bytes,
                    "swap_bytes": m.swap_bytes,
                    "reason": reason,
                    "actual_reader": actual_reader,
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
                let rparen = stat_raw.iter().rposition(|&b| b == b')').ok_or_else(|| {
                    CorexError::InvalidData(
                        "invalid thread stat: missing closing parenthesis".into(),
                    )
                })?;
                let tail_bytes = stat_raw.get(rparen + 1..).ok_or_else(|| {
                    CorexError::InvalidData("invalid thread stat: missing tail".into())
                })?;
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
            let dumped_count = process.mappings.iter().filter(|m| m.should_dump).count();
            let bytes: u64 = process
                .mappings
                .iter()
                .filter(|m| m.should_dump)
                .map(|m| m.end - m.start)
                .sum();
            let required = bytes
                .checked_add(
                    notes.len() as u64 + elf::header_reserve_bytes_for(dumped_count) as u64,
                )
                .ok_or_else(|| CorexError::InvalidData("snapshot size overflow".into()))?;
            if required > output_limit as u64 {
                return Err(CorexError::InvalidData(
                    "mapping growth exceeds output ceiling; detach and report failure".into(),
                ));
            }
            let mut buffered = !stream;
            let mut buffered_fallback = false;
            if buffered && required > limit as u64 {
                // Live mapping growth exceeded pre-stop snapshot buffer, but fits output ceiling:
                // release the Snapshot buffer and fall back cleanly to streaming write.
                storage = None;
                if scratch.is_empty() {
                    scratch = vec![0u8; 1024 * 1024];
                }
                if stream_stats.is_none() {
                    stream_stats = Some(elf::IoStats::reserved_for(process.mappings.len())?);
                }
                buffered = false;
                buffered_fallback = true;
            }
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
                        pagemap.as_ref(),
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
                        pagemap.as_ref(),
                        &check_alive,
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
                buffered_fallback,
                snapshot,
                thread_identities,
                memory_pinned_ns,
                serde_json::json!({"memory_io":io_stats,"procfs_begin_ns":procfs_begin_ns,"procfs_end_ns":procfs_end_ns,"registers_begin_ns":registers_begin_ns,"registers_end_ns":registers_end_ns,"notes_begin_ns":notes_begin_ns,"notes_end_ns":notes_end_ns,"memory_read_begin_ns":memory_read_begin_ns}),
            ))
        })();
        let captured_ns = crate::now();
        let detached = attached.finish();
        let resumed_ns = crate::now();
        let (
            process,
            ranges,
            bytes,
            buffered,
            buffered_fallback,
            snapshot,
            thread_identities,
            memory_pinned_ns,
            stages,
        ) = match (prepared, detached) {
            (Err(e), Err(d)) => return Err(e.with_cleanup(d)),
            (Err(e), _) | (_, Err(e)) => return Err(e),
            (Ok(v), Ok(())) => v,
        };
        Ok((
            serde_json::json!({"allocation_begin_ns":allocation_begin_ns,"allocation_end_ns":allocation_end_ns,"stages":stages,"capture_thread_usage_begin":usage_begin,"capture_thread_usage_end":crate::usage(),"ranges":ranges,"tids":process.tids,"thread_identities":thread_identities,"requested_memory_bytes":bytes,"payload_bytes":stages["memory_io"]["coverage"]["captured_bytes"],"buffered":buffered,"buffered_fallback":buffered_fallback,"actual_reader":if buffered{reader}else{"proc_mem"},"attach_begin_ns":attach_begin,"all_threads_stopped_ns":stopped,"memory_pinned_ns":memory_pinned_ns,"capture_complete_ns":captured_ns,"detach_complete_ns":resumed_ns,"write_complete_ns":null,"writeback":null,"durable_ns":null,"deferred_durability":true,"max_write_mbps":options.max_write_mbps,"pagemap_read_skip_enabled":options.allow_pagemap_skip,"census_state":id.state,"prpsinfo_scheduler_state":"unknown_synthetic_core; use timestamped census_state instead","observed_coredump_filter":process.coredump_filter,"executable":process.exe,"preserved_group_stop":preserved_group_stop,"coverage":stages["memory_io"]["coverage"],"read_errors":stages["memory_io"]["coverage"]["read_errors"],"reserved_output_bytes":output_limit,"reserved_snapshot_bytes":if buffered{limit}else{0},"stream_scratch_bytes":if buffered{0}else{1024*1024}}),
            snapshot,
        ))
    })();
    match result {
        Ok((details, snapshot)) => Ok(Captured {
            details,
            output,
            snapshot,
            max_write_mbps: options.max_write_mbps,
        }),
        Err(error) => match crate::safe_output::remove_owned(path, &output) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.with_cleanup(CorexError::io(
                "remove failed capture",
                path.display().to_string(),
                cleanup,
            ))),
        },
    }
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
            use sha2::{Digest, Sha256};
            const SPARSE_PAGE_BYTES: usize = 4096;
            const MAX_WRITE_BATCH_BYTES: usize = 4 * 1024 * 1024;
            static HASH_ZERO_SLICE: [u8; 65536] = [0u8; 65536];
            let hash_begin = crate::now();
            let bytes = snapshot.bytes();
            let total_len = bytes.len();
            let known_zeros = &snapshot.known_zero_ranges;
            let mut kz_idx = 0usize;
            let mut hasher = Sha256::new();
            let mut offset = 0usize;
            let mut cached_block: Option<(usize, usize, bool)> = None;
            let rate_begin = std::time::Instant::now();
            let max_bps = self.max_write_mbps.map(|m| m as f64 * 1_000_000.0);
            let feed_zero_hash = |hasher: &mut Sha256, mut len: usize| {
                while len > 0 {
                    let step = len.min(HASH_ZERO_SLICE.len());
                    hasher.update(&HASH_ZERO_SLICE[..step]);
                    len -= step;
                }
            };
            let next_zero_probe = |pos: usize, kz: &mut usize| -> (usize, bool) {
                while *kz < known_zeros.len() && known_zeros[*kz].1 <= pos {
                    *kz += 1;
                }
                if *kz < known_zeros.len() && known_zeros[*kz].0 <= pos {
                    return (known_zeros[*kz].1.min(total_len), true);
                }
                let mut end = (pos + SPARSE_PAGE_BYTES).min(total_len);
                if *kz < known_zeros.len() && known_zeros[*kz].0 < end {
                    end = known_zeros[*kz].0;
                }
                (end, false)
            };
            let is_block_zero = |start: usize,
                                 end: usize,
                                 known_zero: bool,
                                 cache: &mut Option<(usize, usize, bool)>,
                                 scan_ns: &mut u128|
             -> bool {
                if known_zero {
                    return true;
                }
                if let Some((cs, ce, val)) = cache.take()
                    && cs == start
                    && ce == end
                {
                    return val;
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
                let (block_end, known_zero) = next_zero_probe(offset, &mut kz_idx);
                if is_block_zero(
                    offset,
                    block_end,
                    known_zero,
                    &mut cached_block,
                    &mut zero_scan_ns,
                ) {
                    let mut hole_end = block_end;
                    while hole_end < total_len {
                        let (next_end, next_known) = next_zero_probe(hole_end, &mut kz_idx);
                        if next_end <= hole_end {
                            break;
                        }
                        if !is_block_zero(
                            hole_end,
                            next_end,
                            next_known,
                            &mut cached_block,
                            &mut zero_scan_ns,
                        ) {
                            cached_block = Some((hole_end, next_end, false));
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
                    feed_zero_hash(&mut hasher, hole_len);
                    offset = hole_end;
                } else {
                    let mut data_end = block_end;
                    while data_end < total_len && (data_end - offset) < MAX_WRITE_BATCH_BYTES {
                        let (next_end, next_known) = next_zero_probe(data_end, &mut kz_idx);
                        if next_end <= data_end || (next_end - offset) > MAX_WRITE_BATCH_BYTES {
                            break;
                        }
                        if is_block_zero(
                            data_end,
                            next_end,
                            next_known,
                            &mut cached_block,
                            &mut zero_scan_ns,
                        ) {
                            if !next_known {
                                cached_block = Some((data_end, next_end, true));
                            }
                            break;
                        }
                        data_end = next_end;
                    }
                    let mut remaining = &bytes[offset..data_end];
                    hasher.update(remaining);
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
                                if let Some(bps) = max_bps {
                                    let expected = written_bytes as f64 / bps;
                                    let elapsed = rate_begin.elapsed().as_secs_f64();
                                    if expected > elapsed {
                                        std::thread::sleep(std::time::Duration::from_secs_f64(
                                            (expected - elapsed).min(0.25),
                                        ));
                                    }
                                }
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
            let hash_end = crate::now();
            self.details["core_sha256"] = serde_json::json!(format!("{:x}", hasher.finalize()));
            self.details["hash_mode"] = serde_json::json!("inline_write");
            self.details["hash_begin_ns"] = serde_json::json!(hash_begin);
            self.details["hash_end_ns"] = serde_json::json!(hash_end);
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
            max_write_mbps: None,
        };
        let token = crate::engine::CancellationToken::with_timeout(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(matches!(
            captured.write(&path, &token, false),
            Err(CorexError::TimedOut)
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_write_4kib_sparse_and_inline_sha256_match_file_bytes() {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let page = 4096usize;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                10 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        struct Region(*mut libc::c_void, usize);
        impl Drop for Region {
            fn drop(&mut self) {
                unsafe { libc::munmap(self.0, self.1) };
            }
        }
        let _region = Region(base, 10 * page);
        let mem_slice = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), 10 * page) };
        mem_slice[0..page].fill(0x55);
        // pages 1..3 demand-zero
        mem_slice[4 * page..5 * page].fill(0); // faulted zero page
        mem_slice[5 * page..6 * page].fill(0x77);
        // pages 6..10 demand-zero trailing hole
        let process = ProcessInfo {
            pid: std::process::id() as i32,
            ppid: 1,
            pgrp: 1,
            sid: 1,
            uid: 0,
            gid: 0,
            page_size: 4096,
            comm: b"test".to_vec(),
            cmdline: b"test".to_vec(),
            auxv: Vec::new(),
            coredump_filter: 0x33,
            exe: "/bin/test".into(),
            tids: vec![std::process::id() as i32],
            mappings: vec![Mapping {
                start: base as u64,
                end: (base as u64) + (10 * page) as u64,
                flags: 6,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: true,
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: Some((3 * page) as u64),
                swap_bytes: Some(0),
                path: "[heap]".into(),
                raw_path: b"[heap]".to_vec(),
            }],
        };
        let memory = File::open("/proc/self/mem").unwrap();
        let pagemap = File::open("/proc/self/pagemap").unwrap();
        let snap = elf::snapshot_reserved(
            &process,
            b"notes",
            None,
            &memory,
            Some(&pagemap),
            "proc_mem",
            &|| Ok(()),
            None,
        )
        .unwrap();
        let expected_sha = format!("{:x}", Sha256::digest(snap.bytes()));
        let expected_len = snap.bytes().len() as u64;
        assert!(!snap.known_zero_ranges.is_empty());

        let path = std::env::temp_dir().join(format!(
            "poddump_inline_sha_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let out = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let captured = Captured {
            details: serde_json::json!({"coverage": snap.stats.coverage.json()}),
            output: out,
            snapshot: Some(snap),
            max_write_mbps: None,
        };
        let token = crate::engine::CancellationToken::with_timeout(10_000);
        let (details, mut file) = captured.write(&path, &token, false).unwrap();
        assert_eq!(details["core_sha256"], expected_sha);
        assert_eq!(details["hash_mode"], "inline_write");
        assert!(details["writer_io"]["sparse_bytes"].as_u64().unwrap() >= (7 * page) as u64);
        let meta = file.metadata().unwrap();
        assert_eq!(meta.len(), expected_len);
        assert!(meta.blocks() * 512 < meta.len());
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&actual)), expected_sha);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn header_reserve_scales_with_dumped_mapping_count() {
        assert!(elf::header_reserve_bytes_for(10) <= 16_384);
        assert!(elf::header_reserve_bytes_for(70_000) > elf::header_reserve_bytes_for(10));
    }
}
