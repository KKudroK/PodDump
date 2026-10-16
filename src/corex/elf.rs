use super::{CorexError, Mapping, ProcessInfo};
use serde::Serialize;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

const ELF_HEADER_SIZE: u64 = 64;
const PROGRAM_HEADER_SIZE: u64 = 56;
const MEMORY_CHUNK_SIZE: usize = 1024 * 1024;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const MAX_RUNS: usize = 65533;
// Reserve the bounded header area before attachment. Failed pages may split a VMA.
pub(super) const HEADER_RESERVE_BYTES: usize = (MAX_RUNS + 1) * 56 + 8192;
fn checked_phnum(loads: usize) -> Result<u16, CorexError> {
    loads
        .checked_add(1)
        .filter(|n| *n < 0xffff)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| {
            CorexError::InvalidData(
                "extended ELF program-header counts are unsupported (maximum 65533 captured runs)"
                    .into(),
            )
        })
}
fn validate_mappings(process: &ProcessInfo, mappings: &[&Mapping]) -> Result<(), CorexError> {
    if !process.page_size.is_power_of_two() {
        return Err(CorexError::InvalidData("invalid ELF page alignment".into()));
    }
    let mut previous = 0;
    for m in mappings {
        if m.start >= m.end
            || m.start < previous
            || m.start % process.page_size != 0
            || m.end % process.page_size != 0
            || m.flags & !7 != 0
        {
            return Err(CorexError::InvalidData(
                "invalid, overlapping, unaligned or unsupported ELF mapping".into(),
            ));
        }
        previous = m.end;
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize)]
pub(super) struct ReadSpan {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub flags: u32,
    pub mapping_index: usize,
}
#[derive(Clone, Debug, Serialize)]
pub(super) struct ReadGap {
    pub start: u64,
    pub end: u64,
    pub errno: Option<i32>,
    pub kind: &'static str,
    pub mapping_index: usize,
}
#[derive(Default, Clone)]
pub(super) struct Coverage {
    pub total_mapped_bytes: u64,
    pub requested_bytes: u64,
    pub excluded_bytes: u64,
    pub excluded_uncommitted_prot_none_bytes: u64,
    pub excluded_kernel_special_bytes: u64,
    pub captured_bytes: u64,
    pub failed_bytes: u64,
    pub zero_chunk_bytes: u64,
    pub spans: Vec<ReadSpan>,
    pub gaps: Vec<ReadGap>,
}
impl Coverage {
    fn reserve() -> Result<Self, CorexError> {
        let mut v = Self::default();
        v.spans
            .try_reserve_exact(MAX_RUNS)
            .map_err(|e| CorexError::InvalidData(format!("reserve captured-run ledger: {e}")))?;
        v.gaps
            .try_reserve_exact(MAX_RUNS)
            .map_err(|e| CorexError::InvalidData(format!("reserve read-error ledger: {e}")))?;
        Ok(v)
    }
    pub(super) fn init_from_process(&mut self, process: &ProcessInfo) {
        let mut total = 0u64;
        let mut requested = 0u64;
        let mut excl_prot_none = 0u64;
        let mut excl_kernel_special = 0u64;

        for m in &process.mappings {
            let bytes = m.end.saturating_sub(m.start);
            total = total.saturating_add(bytes);
            if m.should_dump {
                requested = requested.saturating_add(bytes);
            } else if super::procfs::is_kernel_special_unreadable(&m.path) {
                excl_kernel_special = excl_kernel_special.saturating_add(bytes);
            } else {
                excl_prot_none = excl_prot_none.saturating_add(bytes);
            }
        }

        self.total_mapped_bytes = total;
        self.requested_bytes = requested;
        self.excluded_bytes = total.saturating_sub(requested);
        self.excluded_uncommitted_prot_none_bytes = excl_prot_none;
        self.excluded_kernel_special_bytes = excl_kernel_special;
    }
    pub(super) fn unattempted_bytes(&self) -> u64 {
        self.requested_bytes.saturating_sub(self.captured_bytes + self.failed_bytes)
    }
    pub(super) fn complete(&self) -> bool {
        self.failed_bytes == 0
            && self.captured_bytes == self.requested_bytes
            && self.unattempted_bytes() == 0
    }
    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "policy": "all_mapped_proc_mem",
            "policy_filter": "uncommitted_anonymous_prot_none_and_kernel_special_excluded",
            "complete": self.complete(),
            "total_mapped_bytes": self.total_mapped_bytes,
            "requested_bytes": self.requested_bytes,
            "excluded_bytes": self.excluded_bytes,
            "excluded_uncommitted_prot_none_bytes": self.excluded_uncommitted_prot_none_bytes,
            "excluded_kernel_special_bytes": self.excluded_kernel_special_bytes,
            "captured_bytes": self.captured_bytes,
            "failed_bytes": self.failed_bytes,
            "unattempted_bytes": self.unattempted_bytes(),
            "captured_zero_chunk_bytes": self.zero_chunk_bytes,
            "captured_runs": self.spans,
            "read_errors": self.gaps,
            "missing_representation": "missing bytes have no PT_LOAD coverage; file padding is not captured memory",
            "zero_representation": "sparse holes within PT_LOAD describe successfully read zero bytes only",
            "ledger_entry_limit_each": MAX_RUNS,
        })
    }
    fn success(
        &mut self,
        mi: usize,
        start: u64,
        off: u64,
        flags: u32,
        data: &[u8],
    ) -> Result<(), CorexError> {
        if let Some(last) = self.spans.last_mut() {
            if last.mapping_index == mi
                && last.end == start
                && last.file_offset + (last.end - last.start) == off
            {
                last.end += data.len() as u64;
                self.captured_bytes += data.len() as u64;
                if is_all_zero(data) {
                    self.zero_chunk_bytes += data.len() as u64;
                }
                return Ok(());
            }
        }
        if self.spans.len() == MAX_RUNS {
            return Err(CorexError::InvalidData(
                "captured-run ledger/ELF header limit reached".into(),
            ));
        }
        self.spans.push(ReadSpan {
            start,
            end: start + data.len() as u64,
            file_offset: off,
            flags,
            mapping_index: mi,
        });
        self.captured_bytes += data.len() as u64;
        if is_all_zero(data) {
            self.zero_chunk_bytes += data.len() as u64;
        }
        Ok(())
    }
    fn gap(
        &mut self,
        mi: usize,
        start: u64,
        len: u64,
        errno: Option<i32>,
        kind: &'static str,
    ) -> Result<(), CorexError> {
        if let Some(last) = self.gaps.last_mut() {
            if last.mapping_index == mi
                && last.end == start
                && last.errno == errno
                && last.kind == kind
            {
                last.end += len;
                self.failed_bytes += len;
                return Ok(());
            }
        }
        if self.gaps.len() == MAX_RUNS {
            return Err(CorexError::InvalidData(
                "read-error ledger limit reached".into(),
            ));
        }
        self.gaps.push(ReadGap {
            start,
            end: start + len,
            errno,
            kind,
            mapping_index: mi,
        });
        self.failed_bytes += len;
        Ok(())
    }
}

#[inline]
pub(super) fn is_all_zero(data: &[u8]) -> bool {
    let len = data.len();
    if len == 0 {
        return true;
    }
    if len >= 16 {
        let first = unsafe { (data.as_ptr() as *const u64).read_unaligned() };
        let last = unsafe { (data.as_ptr().add(len - 8) as *const u64).read_unaligned() };
        if (first | last) != 0 {
            return false;
        }
    } else if data[0] != 0 || data[len - 1] != 0 {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            return unsafe { is_all_zero_avx2(data) };
        }
    }
    is_all_zero_scalar(data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn is_all_zero_avx2(data: &[u8]) -> bool {
    use std::arch::x86_64::*;
    let mut ptr = data.as_ptr();
    let mut rem = data.len();
    while rem >= 128 {
        let v0 = unsafe { _mm256_loadu_si256(ptr as *const __m256i) };
        let v1 = unsafe { _mm256_loadu_si256(ptr.add(32) as *const __m256i) };
        let v2 = unsafe { _mm256_loadu_si256(ptr.add(64) as *const __m256i) };
        let v3 = unsafe { _mm256_loadu_si256(ptr.add(96) as *const __m256i) };
        let or01 = _mm256_or_si256(v0, v1);
        let or23 = _mm256_or_si256(v2, v3);
        let combined = _mm256_or_si256(or01, or23);
        if _mm256_testz_si256(combined, combined) == 0 {
            return false;
        }
        ptr = unsafe { ptr.add(128) };
        rem -= 128;
    }
    while rem >= 32 {
        let v = unsafe { _mm256_loadu_si256(ptr as *const __m256i) };
        if _mm256_testz_si256(v, v) == 0 {
            return false;
        }
        ptr = unsafe { ptr.add(32) };
        rem -= 32;
    }
    while rem >= 8 {
        if unsafe { (ptr as *const u64).read_unaligned() } != 0 {
            return false;
        }
        ptr = unsafe { ptr.add(8) };
        rem -= 8;
    }
    while rem > 0 {
        if unsafe { *ptr } != 0 {
            return false;
        }
        ptr = unsafe { ptr.add(1) };
        rem -= 1;
    }
    true
}

#[inline]
fn is_all_zero_scalar(data: &[u8]) -> bool {
    let mut ptr = data.as_ptr();
    let mut rem = data.len();
    while rem >= 32 {
        let w0 = unsafe { (ptr as *const u64).read_unaligned() };
        let w1 = unsafe { (ptr.add(8) as *const u64).read_unaligned() };
        let w2 = unsafe { (ptr.add(16) as *const u64).read_unaligned() };
        let w3 = unsafe { (ptr.add(24) as *const u64).read_unaligned() };
        if (w0 | w1 | w2 | w3) != 0 {
            return false;
        }
        ptr = unsafe { ptr.add(32) };
        rem -= 32;
    }
    while rem >= 8 {
        if unsafe { (ptr as *const u64).read_unaligned() } != 0 {
            return false;
        }
        ptr = unsafe { ptr.add(8) };
        rem -= 8;
    }
    while rem > 0 {
        if unsafe { *ptr } != 0 {
            return false;
        }
        ptr = unsafe { ptr.add(1) };
        rem -= 1;
    }
    true
}

#[derive(Default, Clone)]
pub(super) struct IoStats {
    read_calls: u64,
    read_bytes: u64,
    short_reads: u64,
    read_ns: u128,
    write_calls: u64,
    write_ns: u128,
    pub coverage: Coverage,
}
impl IoStats {
    pub(super) fn reserved() -> Result<Self, CorexError> {
        Ok(Self {
            coverage: Coverage::reserve()?,
            ..Self::default()
        })
    }
    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::json!({"read_calls":self.read_calls,"read_bytes":self.read_bytes,"short_reads":self.short_reads,"read_syscall_ns":self.read_ns,"stream_logical_write_calls":self.write_calls,"stream_write_ns":self.write_ns,"timings_enabled":crate::TRACE.load(crate::Ordering::Relaxed),"coverage":self.coverage.json()})
    }
}
pub(super) struct Snapshot {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
    pub(super) stats: IoStats,
}
unsafe impl Send for Snapshot {}
impl Snapshot {
    pub(super) fn allocate(capacity: usize) -> Result<Self, CorexError> {
        if capacity == 0 || capacity > isize::MAX as usize {
            return Err(CorexError::InvalidData("invalid snapshot capacity".into()));
        }
        let stats = IoStats::reserved()?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(CorexError::io(
                "allocate snapshot",
                "anonymous mmap",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(Self {
            ptr: ptr.cast(),
            len: capacity,
            capacity,
            stats,
        })
    }
    pub(super) fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.capacity);
        }
    }
}
pub(super) struct SparseWriter<'a> {
    pub file: &'a mut File,
}
impl Write for SparseWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() >= 4096 && is_all_zero(data) {
            self.file.seek(SeekFrom::Current(data.len() as i64))?;
            Ok(data.len())
        } else {
            self.file.write(data)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let len = self.file.stream_position()?;
        self.file.set_len(len)?;
        self.file.flush()
    }
}
impl Seek for SparseWriter<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}
struct Layout {
    note: u64,
    first: u64,
    end: u64,
}
fn layout(process: &ProcessInfo, notes: &[u8]) -> Result<Layout, CorexError> {
    let dumped: Vec<_> = process.mappings.iter().filter(|m| m.should_dump).collect();
    checked_phnum(dumped.len())?;
    validate_mappings(process, &dumped)?;
    let note = ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE * (MAX_RUNS as u64 + 1);
    let first = align_page(
        note.checked_add(notes.len() as u64)
            .ok_or_else(|| CorexError::InvalidData("ELF note size overflow".into()))?,
        process.page_size,
    )?;
    let end = dumped.iter().try_fold(first, |n, m| {
        n.checked_add(mapping_size(m)?)
            .ok_or_else(|| CorexError::InvalidData("ELF payload size overflow".into()))
    })?;
    Ok(Layout { note, first, end })
}
// Retrying a failed large read at the current page distinguishes a bad page
// from readable pages later in the same VMA. Only successful bytes reach sink.
fn read_mapping<F: FnMut(u64, &[u8]) -> Result<(), CorexError>>(
    memory: &File,
    m: &Mapping,
    mi: usize,
    offset: u64,
    page: u64,
    buffer: &mut [u8],
    stats: &mut IoStats,
    cancellation: Option<&crate::engine::CancellationToken>,
    alive: &dyn Fn() -> Result<(), CorexError>,
    mut sink: F,
) -> Result<(), CorexError> {
    let mut address = m.start;
    while address < m.end {
        check_cancelled(cancellation)?;
        alive()?;
        let mut requested = ((m.end - address) as usize).min(buffer.len());
        let mut page_retry = false;
        loop {
            check_cancelled(cancellation)?;
            let detailed = crate::TRACE.load(crate::Ordering::Relaxed);
            let begin = if detailed { crate::now() } else { 0 };
            let got = memory.read_at(&mut buffer[..requested], address);
            stats.read_calls += 1;
            if detailed {
                stats.read_ns += crate::now() - begin;
            }
            match got {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(n) if n > 0 => {
                    let off = offset + address - m.start;
                    stats
                        .coverage
                        .success(mi, address, off, m.flags, &buffer[..n])?;
                    sink(off, &buffer[..n])?;
                    stats.read_bytes += n as u64;
                    stats.short_reads += u64::from(n < requested);
                    address += n as u64;
                    break;
                }
                other => {
                    let page_remaining = (page - address % page).min(m.end - address);
                    if !page_retry && requested as u64 > page_remaining {
                        requested = page_remaining as usize;
                        page_retry = true;
                        continue;
                    }
                    let (errno, kind) = match other {
                        Err(e) => (e.raw_os_error(), "read_error"),
                        Ok(0) => (None, "zero_length_read"),
                        _ => unreachable!(),
                    };
                    stats
                        .coverage
                        .gap(mi, address, page_remaining, errno, kind)?;
                    address += page_remaining;
                    break;
                }
            }
        }
    }
    Ok(())
}
fn headers(
    process: &ProcessInfo,
    notes: &[u8],
    l: &Layout,
    c: &Coverage,
) -> Result<Vec<u8>, CorexError> {
    let mut bytes = Vec::with_capacity(64 + 56 * (c.spans.len() + 1));
    bytes.extend_from_slice(&elf_header(checked_phnum(c.spans.len())?));
    bytes.extend_from_slice(&program_header(
        PT_NOTE,
        0,
        l.note,
        0,
        notes.len() as u64,
        0,
        4,
    ));
    for span in &c.spans {
        bytes.extend_from_slice(&program_header(
            PT_LOAD,
            span.flags,
            span.file_offset,
            span.start,
            span.end - span.start,
            span.end - span.start,
            process.page_size,
        ));
    }
    Ok(bytes)
}
#[cfg(test)]
pub(super) fn snapshot(
    process: &ProcessInfo,
    notes: &[u8],
    c: Option<&crate::engine::CancellationToken>,
    memory: &File,
    reader: &str,
    alive: &dyn Fn() -> Result<(), CorexError>,
) -> Result<Snapshot, CorexError> {
    snapshot_reserved(process, notes, c, memory, reader, alive, None)
}
pub(super) fn snapshot_reserved(
    process: &ProcessInfo,
    notes: &[u8],
    c: Option<&crate::engine::CancellationToken>,
    memory: &File,
    _reader: &str,
    alive: &dyn Fn() -> Result<(), CorexError>,
    storage: Option<Snapshot>,
) -> Result<Snapshot, CorexError> {
    let l = layout(process, notes)?;
    let len =
        usize::try_from(l.end).map_err(|_| CorexError::InvalidData("snapshot too large".into()))?;
    let mut s = match storage {
        Some(s) => s,
        None => Snapshot::allocate(len)?,
    };
    if len > s.capacity {
        return Err(CorexError::InvalidData(
            "mapping growth exceeds pre-stop snapshot reservation; detached without waiting".into(),
        ));
    }
    s.len = len;
    s.stats.coverage.init_from_process(process);
    let data = unsafe { std::slice::from_raw_parts_mut(s.ptr, s.len) };
    data[l.note as usize..l.note as usize + notes.len()].copy_from_slice(notes);
    // Read directly into a bounded scratch slice of the reserved snapshot;
    // successful data is copied only when read_at's destination differs.
    let mut offset = l.first;
    let result = (|| {
        for (mi, m) in process
            .mappings
            .iter()
            .enumerate()
            .filter(|(_, m)| m.should_dump)
        {
            let size = mapping_size(m)? as usize;
            // Use this mapping's own reserved bytes as scratch: failed reads
            // never acquire a PT_LOAD and no zero is substituted as evidence.
            let mut cursor = 0usize;
            while cursor < size {
                check_cancelled(c)?;
                alive()?;
                let mut n = (size - cursor).min(MEMORY_CHUNK_SIZE);
                let mut retry = false;
                loop {
                    check_cancelled(c)?;
                    let at = m.start + cursor as u64;
                    let detailed = crate::TRACE.load(crate::Ordering::Relaxed);
                    let begin = if detailed { crate::now() } else { 0 };
                    let got = memory.read_at(
                        &mut data[offset as usize + cursor..offset as usize + cursor + n],
                        at,
                    );
                    s.stats.read_calls += 1;
                    if detailed {
                        s.stats.read_ns += crate::now() - begin;
                    }
                    match got {
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Ok(got) if got > 0 => {
                            s.stats.coverage.success(
                                mi,
                                at,
                                offset + cursor as u64,
                                m.flags,
                                &data[offset as usize + cursor..offset as usize + cursor + got],
                            )?;
                            s.stats.read_bytes += got as u64;
                            s.stats.short_reads += u64::from(got < n);
                            cursor += got;
                            break;
                        }
                        other => {
                            let missing = (process.page_size - at % process.page_size)
                                .min((size - cursor) as u64);
                            if !retry && n as u64 > missing {
                                n = missing as usize;
                                retry = true;
                                continue;
                            }
                            let (errno, kind) = match other {
                                Err(e) => (e.raw_os_error(), "read_error"),
                                Ok(0) => (None, "zero_length_read"),
                                _ => unreachable!(),
                            };
                            s.stats.coverage.gap(mi, at, missing, errno, kind)?;
                            cursor += missing as usize;
                            break;
                        }
                    }
                }
            }
            offset += size as u64;
        }
        Ok::<_, CorexError>(())
    })();
    result.map_err(|e| e.with_coverage(s.stats.coverage.json()))?;
    let head = headers(process, notes, &l, &s.stats.coverage)?;
    data[..head.len()].copy_from_slice(&head);
    Ok(s)
}
#[cfg(test)]
pub(super) fn write<W: Write + Seek>(
    path: &Path,
    output: W,
    process: &ProcessInfo,
    notes: &[u8],
    c: Option<&crate::engine::CancellationToken>,
    memory: &File,
) -> Result<(), CorexError> {
    write_reserved(
        path,
        output,
        process,
        notes,
        c,
        memory,
        &mut vec![0; MEMORY_CHUNK_SIZE],
        None,
    )
    .map(|_| ())
}
pub(super) fn write_reserved<W: Write + Seek>(
    path: &Path,
    mut output: W,
    process: &ProcessInfo,
    notes: &[u8],
    c: Option<&crate::engine::CancellationToken>,
    memory: &File,
    buffer: &mut [u8],
    stats: Option<IoStats>,
) -> Result<IoStats, CorexError> {
    let l = layout(process, notes)?;
    let mut stats = match stats {
        Some(s) => s,
        None => IoStats::reserved()?,
    };
    stats.coverage.init_from_process(process);
    assert_eq!(buffer.len(), MEMORY_CHUNK_SIZE);
    let display = path.display().to_string();
    output
        .seek(SeekFrom::Start(l.note))
        .and_then(|_| output.write_all(notes))
        .map_err(|e| CorexError::io("write notes", &display, e))?;
    let mut offset = l.first;
    let result = (|| {
        for (mi, m) in process
            .mappings
            .iter()
            .enumerate()
            .filter(|(_, m)| m.should_dump)
        {
            let mut writes = 0u64;
            let mut elapsed = 0u128;
            read_mapping(
                memory,
                m,
                mi,
                offset,
                process.page_size,
                buffer,
                &mut stats,
                c,
                &|| Ok(()),
                |off, bytes| {
                    let begin = crate::now();
                    let result = output
                        .seek(SeekFrom::Start(off))
                        .and_then(|_| output.write_all(bytes))
                        .map_err(|e| CorexError::io("write captured bytes", &display, e));
                    writes += 1;
                    elapsed += crate::now() - begin;
                    result
                },
            )?;
            stats.write_calls += writes;
            stats.write_ns += elapsed;
            offset += mapping_size(m)?;
        }
        Ok::<_, CorexError>(())
    })();
    result.map_err(|e| e.with_coverage(stats.coverage.json()))?;
    let head = headers(process, notes, &l, &stats.coverage)?;
    output
        .seek(SeekFrom::Start(0))
        .and_then(|_| output.write_all(&head))
        .and_then(|_| output.seek(SeekFrom::Start(l.end)))
        .and_then(|_| output.flush())
        .map_err(|e| CorexError::io("finalize core layout", &display, e))?;
    Ok(stats)
}
fn elf_header(program_header_count: u16) -> [u8; ELF_HEADER_SIZE as usize] {
    let mut header = [0_u8; ELF_HEADER_SIZE as usize];
    header[0..4].copy_from_slice(b"\x7fELF");
    header[4] = 2;
    header[5] = 1;
    header[6] = 1;
    put_u16(&mut header, 16, 4);
    put_u16(&mut header, 18, elf_machine());
    put_u32(&mut header, 20, 1);
    put_u64(&mut header, 32, ELF_HEADER_SIZE);
    put_u16(&mut header, 52, ELF_HEADER_SIZE as u16);
    put_u16(&mut header, 54, PROGRAM_HEADER_SIZE as u16);
    put_u16(&mut header, 56, program_header_count);
    header
}

fn program_header(
    segment_type: u32,
    flags: u32,
    offset: u64,
    virtual_address: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
) -> [u8; PROGRAM_HEADER_SIZE as usize] {
    let mut header = [0_u8; PROGRAM_HEADER_SIZE as usize];
    put_u32(&mut header, 0, segment_type);
    put_u32(&mut header, 4, flags);
    put_u64(&mut header, 8, offset);
    put_u64(&mut header, 16, virtual_address);
    put_u64(&mut header, 32, file_size);
    put_u64(&mut header, 40, memory_size);
    put_u64(&mut header, 48, alignment);
    header
}

fn check_cancelled(
    cancellation: Option<&crate::engine::CancellationToken>,
) -> Result<(), CorexError> {
    if cancellation.is_some_and(crate::engine::CancellationToken::is_cancelled) {
        if cancellation.is_some_and(crate::engine::CancellationToken::timed_out) {
            Err(CorexError::TimedOut)
        } else {
            Err(CorexError::Cancelled)
        }
    } else {
        Ok(())
    }
}

fn mapping_size(mapping: &Mapping) -> Result<u64, CorexError> {
    mapping.end.checked_sub(mapping.start).ok_or_else(|| {
        CorexError::InvalidData(format!(
            "invalid process mapping {:#x}-{:#x}",
            mapping.start, mapping.end
        ))
    })
}

fn align_page(value: u64, page_size: u64) -> Result<u64, CorexError> {
    if !page_size.is_power_of_two() {
        return Err(CorexError::InvalidData("invalid ELF page alignment".into()));
    }
    value
        .checked_add(page_size - 1)
        .map(|value| value / page_size * page_size)
        .ok_or_else(|| CorexError::InvalidData("ELF alignment overflow".into()))
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(target_arch = "aarch64")]
const fn elf_machine() -> u16 {
    183
}

#[cfg(target_arch = "x86_64")]
const fn elf_machine() -> u16 {
    62
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn process() -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            ppid: 0,
            pgrp: 1,
            sid: 1,
            uid: 0,
            gid: 0,
            comm: b"test".to_vec(),
            exe: "/test".into(),
            cmdline: b"test\0".to_vec(),
            mappings: vec![Mapping {
                start: 0x1000,
                end: 0x2000,
                flags: 4,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: true,
                path: String::new(),
                raw_path: Vec::new(),
            }],
            auxv: vec![],
            coredump_filter: 0x32,
            page_size: 4096,
            tids: vec![1],
        }
    }

    #[test]
    fn reserved_extended_count_is_rejected_by_both_writers() {
        assert_eq!(checked_phnum(0).unwrap(), 1);
        assert_eq!(checked_phnum(65533).unwrap(), 65534);
        assert!(checked_phnum(65534).is_err());
        assert!(checked_phnum(usize::MAX).is_err());
        let mut process = process();
        process.mappings = vec![process.mappings[0].clone(); 65534];
        let memory = File::open("/dev/null").unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        let error = write(
            Path::new("unused"),
            &mut output,
            &process,
            &[],
            None,
            &memory,
        )
        .unwrap_err();
        assert!(error.to_string().contains("extended ELF program-header"));
        assert!(output.get_ref().is_empty());
        match snapshot(&process, &[], None, &memory, "proc_mem", &|| Ok(())) {
            Err(error) => assert!(error.to_string().contains("extended ELF program-header")),
            Ok(_) => panic!("reserved PN_XNUM was emitted without section headers"),
        }
    }

    #[test]
    fn invalid_layout_is_rejected_before_any_output() {
        let memory = File::open("/dev/null").unwrap();
        let mut cases = Vec::new();
        let mut p = process();
        p.page_size = 3;
        cases.push(p);
        let mut p = process();
        p.page_size = 0;
        cases.push(p);
        let mut p = process();
        p.mappings[0].start += 1;
        cases.push(p);
        let mut p = process();
        p.mappings[0].end = p.mappings[0].start;
        cases.push(p);
        let mut p = process();
        p.mappings[0].flags = 8;
        cases.push(p);
        let mut p = process();
        p.mappings.push(p.mappings[0].clone());
        cases.push(p);
        for p in cases {
            let mut output = std::io::Cursor::new(Vec::new());
            assert!(write(Path::new("unused"), &mut output, &p, &[], None, &memory).is_err());
            assert!(output.get_ref().is_empty());
            assert!(snapshot(&p, &[], None, &memory, "proc_mem", &|| Ok(())).is_err());
        }
        assert!(align_page(u64::MAX, 4096).is_err());
    }

    #[test]
    fn stream_and_snapshot_layouts_match_and_short_reads_fail() {
        let process = process();
        let zero = File::open("/dev/zero").unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        write(Path::new("unused"), &mut output, &process, &[], None, &zero).unwrap();
        let buffered = snapshot(&process, &[], None, &zero, "proc_mem", &|| Ok(())).unwrap();
        let output = output.into_inner();
        assert_eq!(output, buffered.bytes());
        assert_eq!(output.len(), layout(&process, &[]).unwrap().end as usize);
        assert_eq!(u16::from_le_bytes(output[56..58].try_into().unwrap()), 2);
        assert_eq!(
            u64::from_le_bytes(output[128..136].try_into().unwrap()),
            layout(&process, &[]).unwrap().first
        );
        let empty = File::open("/dev/null").unwrap();
        let mut absent = std::io::Cursor::new(Vec::new());
        write(
            Path::new("unused"),
            &mut absent,
            &process,
            &[],
            None,
            &empty,
        )
        .unwrap();
        assert_eq!(
            u16::from_le_bytes(absent.get_ref()[56..58].try_into().unwrap()),
            1
        );
        let partial = snapshot(&process, &[], None, &empty, "proc_mem", &|| Ok(())).unwrap();
        assert!(!partial.stats.coverage.complete());
        assert_eq!(partial.stats.coverage.failed_bytes, 4096);
        assert!(partial.stats.coverage.spans.is_empty());
    }

    #[test]
    fn sparse_zero_tail_keeps_logical_length() {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::memfd_create(c"poddump_sparse_test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut file = unsafe { File::from_raw_fd(fd) };
        {
            let mut writer = SparseWriter { file: &mut file };
            writer.write_all(&[0x41; 4096]).unwrap();
            writer.write_all(&[0; 8192]).unwrap();
            writer.flush().unwrap();
        }
        assert_eq!(file.metadata().unwrap().len(), 12288);
        let mut tail = vec![1; 8192];
        assert_eq!(file.read_at(&mut tail, 4096).unwrap(), 8192);
        assert!(tail.iter().all(|b| *b == 0));
    }

    #[test]
    fn core_header_has_expected_elf_identity() {
        let header = elf_header(3);
        assert_eq!(&header[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes(header[16..18].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(header[56..58].try_into().unwrap()), 3);
    }

    #[test]
    fn page_alignment_rounds_up() {
        assert_eq!(align_page(16_384, 16_384).unwrap(), 16_384);
        assert_eq!(align_page(16_385, 16_384).unwrap(), 32_768);
    }
}

#[cfg(test)]
mod maximum_coverage_regressions {
    use super::*;
    use std::os::fd::FromRawFd;
    struct Region(*mut libc::c_void, usize);
    impl Drop for Region {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.0, self.1);
            }
        }
    }
    fn read_elf(core: &[u8], address: u64) -> Option<Vec<u8>> {
        let phoff = u64::from_le_bytes(core[32..40].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(core[56..58].try_into().unwrap()) as usize;
        for n in 0..count {
            let p = &core[phoff + 56 * n..phoff + 56 * n + 56];
            if u32::from_le_bytes(p[0..4].try_into().unwrap()) != PT_LOAD {
                continue;
            }
            let off = u64::from_le_bytes(p[8..16].try_into().unwrap());
            let start = u64::from_le_bytes(p[16..24].try_into().unwrap());
            let size = u64::from_le_bytes(p[32..40].try_into().unwrap());
            if address >= start && address + 4096 <= start + size {
                let at = (off + address - start) as usize;
                return Some(core[at..at + 4096].to_vec());
            }
        }
        None
    }
    #[test]
    fn proc_mem_preserves_nonreadable_bytes_and_skips_actual_faults_without_zero_fabrication() {
        let page = 4096usize;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                7 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let _region = Region(base, 7 * page);
        let bytes = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), 7 * page) };
        for i in 0..7 {
            bytes[i * page..(i + 1) * page].fill(0x41 + i as u8);
        }
        bytes[5 * page..6 * page].fill(0);
        assert_eq!(
            unsafe { libc::mprotect((base as usize + page) as *mut _, page, libc::PROT_NONE) },
            0
        );
        assert_eq!(
            unsafe { libc::mprotect((base as usize + 2 * page) as *mut _, page, libc::PROT_WRITE) },
            0
        );
        let fd = unsafe { libc::memfd_create(c"coverage_eof".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let backing = unsafe { File::from_raw_fd(fd) };
        backing.set_len(page as u64).unwrap();
        backing.write_all_at(&vec![0x44; page], 0).unwrap();
        assert_eq!(
            unsafe {
                libc::mmap(
                    (base as usize + 3 * page) as *mut _,
                    2 * page,
                    libc::PROT_READ,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    fd,
                    0,
                )
            },
            (base as usize + 3 * page) as *mut _
        );
        let mut process = tests::process();
        process.pid = std::process::id() as i32;
        process.mappings.clear();
        let start = base as u64;
        for (i, length, flags) in [
            (0, 1, 4),
            (1, 1, 0),
            (2, 1, 2),
            (3, 2, 4),
            (5, 1, 4),
            (6, 1, 4),
        ] {
            process.mappings.push(Mapping {
                start: start + (i * page) as u64,
                end: start + ((i + length) * page) as u64,
                flags,
                offset: 0,
                is_shared: i == 3,
                is_file_backed: i == 3,
                should_dump: true,
                path: format!("region_{i}"),
                raw_path: format!("region_{i}").into_bytes(),
            });
        }
        let memory = File::open("/proc/self/mem").unwrap();
        let buffered = snapshot(&process, &[], None, &memory, "proc_mem", &|| Ok(())).unwrap();
        assert_eq!(buffered.stats.coverage.requested_bytes, 7 * page as u64);
        assert_eq!(buffered.stats.coverage.captured_bytes, 6 * page as u64);
        assert_eq!(buffered.stats.coverage.failed_bytes, page as u64);
        assert_eq!(buffered.stats.coverage.zero_chunk_bytes, page as u64);
        assert_eq!(buffered.stats.coverage.gaps[0].errno, Some(libc::EIO));
        assert!(!buffered.stats.coverage.complete());
        for i in [0, 1, 2, 3, 5, 6] {
            let expected = if i == 5 { 0 } else { 0x41 + i as u8 };
            assert_eq!(
                read_elf(buffered.bytes(), start + (i * page) as u64).unwrap(),
                vec![expected; page]
            );
        }
        assert!(read_elf(buffered.bytes(), start + (4 * page) as u64).is_none());
        let fd = unsafe { libc::memfd_create(c"coverage_stream".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut out = unsafe { File::from_raw_fd(fd) };
        let mut scratch = vec![0; MEMORY_CHUNK_SIZE];
        let stats = write_reserved(
            Path::new("memfd"),
            SparseWriter { file: &mut out },
            &process,
            &[],
            None,
            &memory,
            &mut scratch,
            None,
        )
        .unwrap();
        assert_eq!(stats.coverage.json(), buffered.stats.coverage.json());
        let mut stream = vec![0; out.metadata().unwrap().len() as usize];
        out.read_exact_at(&mut stream, 0).unwrap();
        assert_eq!(stream, buffered.bytes());
    }
    #[test]
    fn simd_zero_detection_matches_scalar_across_alignments_and_sizes() {
        for size in [0usize, 1, 7, 15, 16, 31, 32, 63, 64, 127, 128, 255, 4096, 65536] {
            let mut buf = vec![0u8; size];
            assert!(is_all_zero(&buf));
            assert!(is_all_zero_scalar(&buf));
            for idx in [0, size / 2, size.saturating_sub(1)] {
                if idx < size {
                    buf[idx] = 0x80;
                    assert!(!is_all_zero(&buf));
                    assert!(!is_all_zero_scalar(&buf));
                    buf[idx] = 0;
                }
            }
        }
        let process = tests::process();
        let mem = File::open("/dev/zero").unwrap();
        let snap = snapshot_reserved(&process, b"note-bytes", None, &mem, "proc_mem", &|| Ok(()), None)
            .unwrap();
        let l = layout(&process, b"note-bytes").unwrap();
        let bytes = snap.bytes();
        assert!(is_all_zero(&bytes[176..l.note as usize]));
        assert!(is_all_zero(
            &bytes[l.note as usize + b"note-bytes".len()..l.first as usize]
        ));
    }

    #[test]
    fn coverage_accounting_records_total_and_excluded_breakdown() {
        let mut process = tests::process();
        let page = process.page_size;
        process.mappings = vec![
            Mapping {
                start: 0x1000,
                end: 0x1000 + page,
                flags: 4,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: true,
                path: "[heap]".into(),
                raw_path: b"[heap]".to_vec(),
            },
            Mapping {
                start: 0x2000,
                end: 0x2000 + page * 4,
                flags: 0,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: false,
                path: "".into(),
                raw_path: Vec::new(),
            },
            Mapping {
                start: 0x7fff0000,
                end: 0x7fff0000 + page * 2,
                flags: 4,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: false,
                path: "[vvar]".into(),
                raw_path: b"[vvar]".to_vec(),
            },
        ];
        let mut coverage = Coverage::reserve().unwrap();
        coverage.init_from_process(&process);
        assert_eq!(coverage.total_mapped_bytes, 7 * page);
        assert_eq!(coverage.requested_bytes, page);
        assert_eq!(coverage.excluded_bytes, 6 * page);
        assert_eq!(coverage.excluded_uncommitted_prot_none_bytes, 4 * page);
        assert_eq!(coverage.excluded_kernel_special_bytes, 2 * page);
        assert_eq!(
            coverage.total_mapped_bytes,
            coverage.requested_bytes + coverage.excluded_bytes
        );
        let j = coverage.json();
        assert_eq!(j["policy"], "all_mapped_proc_mem");
        assert_eq!(
            j["policy_filter"],
            "uncommitted_anonymous_prot_none_and_kernel_special_excluded"
        );
        assert_eq!(j["total_mapped_bytes"], 7 * page);
        assert_eq!(j["requested_bytes"], page);
        assert_eq!(j["excluded_bytes"], 6 * page);
        assert_eq!(j["excluded_uncommitted_prot_none_bytes"], 4 * page);
        assert_eq!(j["excluded_kernel_special_bytes"], 2 * page);
    }
}
