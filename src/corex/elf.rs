use super::{CorexError, Mapping, ProcessInfo};
use serde::Serialize;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

const ELF_HEADER_SIZE: u64 = 64;
const PROGRAM_HEADER_SIZE: u64 = 56;
const ELF_SECTION_HEADER_SIZE: u64 = 64;
const PN_XNUM: u16 = 0xffff;
const MEMORY_CHUNK_SIZE: usize = 1024 * 1024;
const SPARSE_PAGE_BYTES: usize = 4096;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const MAX_PHDRS: usize = 524_288;
const MIN_PHDR_SLACK: usize = 64;
const MAX_PHDR_SLACK: usize = 1024;
// Guard/userfaultfd markers are not demand-zero pages, even when non-present.
const PM_NONZERO_BITS: u64 = (1 << 63) | (1 << 62) | (1 << 61) | (1 << 58) | (1 << 57);

fn phdr_capacity_for(dumped_mappings: usize, mapped_bytes: u64) -> Result<usize, CorexError> {
    if dumped_mappings >= MAX_PHDRS {
        return Err(CorexError::InvalidData(
            "too many mapped regions for ELF headers".into(),
        ));
    }
    // One failed page can split readable bytes into another PT_LOAD. Reserve
    // for alternating readable/unreadable pages, not only the VMA count.
    // The unused header area is sparse; the global ledger limit remains explicit.
    let runs = dumped_mappings.saturating_add((mapped_bytes / 4096).div_ceil(2) as usize);
    let slack = dumped_mappings.clamp(MIN_PHDR_SLACK, MAX_PHDR_SLACK);
    Ok(runs.saturating_add(1 + slack).min(MAX_PHDRS))
}

pub(super) fn header_reserve_bytes_for(dumped_mappings: usize, mapped_bytes: u64) -> usize {
    let slots = phdr_capacity_for(dumped_mappings, mapped_bytes).unwrap_or(MAX_PHDRS);
    (ELF_HEADER_SIZE as usize)
        .saturating_add(slots.saturating_mul(PROGRAM_HEADER_SIZE as usize))
        .saturating_add(ELF_SECTION_HEADER_SIZE as usize)
        .saturating_add(8192)
}

fn checked_phnum_fields(loads: usize) -> Result<(u16, Option<u32>), CorexError> {
    let real = loads
        .checked_add(1)
        .filter(|n| *n <= MAX_PHDRS && *n <= u32::MAX as usize)
        .ok_or_else(|| {
            CorexError::InvalidData(format!(
                "ELF program-header count exceeds maximum supported runs ({MAX_PHDRS})"
            ))
        })?;
    if real < PN_XNUM as usize {
        Ok((real as u16, None))
    } else {
        Ok((PN_XNUM, Some(real as u32)))
    }
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
    pub excluded_device_or_pfnmap_bytes: u64,
    pub captured_bytes: u64,
    pub failed_bytes: u64,
    pub zero_chunk_bytes: u64,
    pub pagemap_skipped_bytes: u64,
    pub stop_timeout_reached: bool,
    pub spans: Vec<ReadSpan>,
    pub gaps: Vec<ReadGap>,
}
impl Coverage {
    #[cfg(test)]
    fn reserve() -> Result<Self, CorexError> {
        Self::reserve_for(256)
    }
    pub(super) fn reserve_for(expected_mappings: usize) -> Result<Self, CorexError> {
        let mut v = Self::default();
        let cap = expected_mappings.clamp(64, 4096);
        v.spans
            .try_reserve_exact(cap)
            .map_err(|e| CorexError::InvalidData(format!("reserve captured-run ledger: {e}")))?;
        v.gaps
            .try_reserve_exact(cap.min(256))
            .map_err(|e| CorexError::InvalidData(format!("reserve read-error ledger: {e}")))?;
        Ok(v)
    }
    pub(super) fn init_from_process(&mut self, process: &ProcessInfo) {
        let mut total = 0u64;
        let mut requested = 0u64;
        let mut excl_prot_none = 0u64;
        let mut excl_kernel_special = 0u64;
        let mut excl_device_or_pfnmap = 0u64;

        for m in &process.mappings {
            let bytes = m.end.saturating_sub(m.start);
            total = total.saturating_add(bytes);
            if m.should_dump {
                requested = requested.saturating_add(bytes);
            } else if m.exclusion_reason == Some("device_io_or_pfnmap") {
                excl_device_or_pfnmap = excl_device_or_pfnmap.saturating_add(bytes);
            } else if m.exclusion_reason == Some("kernel_special_unreadable")
                || super::procfs::is_kernel_special_unreadable(&m.path)
            {
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
        self.excluded_device_or_pfnmap_bytes = excl_device_or_pfnmap;
    }
    pub(super) fn unattempted_bytes(&self) -> u64 {
        self.requested_bytes
            .saturating_sub(self.captured_bytes + self.failed_bytes)
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
            "excluded_device_or_pfnmap_bytes": self.excluded_device_or_pfnmap_bytes,
            "captured_bytes": self.captured_bytes,
            "failed_bytes": self.failed_bytes,
            "unattempted_bytes": self.unattempted_bytes(),
            "stop_timeout_reached": self.stop_timeout_reached,
            "captured_zero_chunk_bytes": self.zero_chunk_bytes,
            "pagemap_skipped_bytes": self.pagemap_skipped_bytes,
            "captured_runs": self.spans,
            "read_errors": self.gaps,
            "missing_representation": "missing bytes have no PT_LOAD coverage; file padding is not captured memory",
            "zero_representation": "sparse holes within PT_LOAD describe successfully read or kernel-verified unpopulated zero bytes only",
            "ledger_entry_limit_each": MAX_PHDRS,
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn record_span(
        &mut self,
        mi: usize,
        start: u64,
        off: u64,
        flags: u32,
        len: u64,
        is_zero: bool,
        pagemap_skipped: bool,
    ) -> Result<(), CorexError> {
        if let Some(last) = self.spans.last_mut()
            && last.mapping_index == mi
            && last.end == start
            && last.file_offset + (last.end - last.start) == off
        {
            last.end += len;
            self.captured_bytes += len;
            if is_zero {
                self.zero_chunk_bytes += len;
            }
            if pagemap_skipped {
                self.pagemap_skipped_bytes += len;
            }
            return Ok(());
        }
        if self.spans.len() >= MAX_PHDRS - 1 {
            return Err(CorexError::InvalidData(
                "captured-run ledger/ELF header limit reached".into(),
            ));
        }
        self.spans.push(ReadSpan {
            start,
            end: start + len,
            file_offset: off,
            flags,
            mapping_index: mi,
        });
        self.captured_bytes += len;
        if is_zero {
            self.zero_chunk_bytes += len;
        }
        if pagemap_skipped {
            self.pagemap_skipped_bytes += len;
        }
        Ok(())
    }
    fn success(
        &mut self,
        mi: usize,
        start: u64,
        off: u64,
        flags: u32,
        data: &[u8],
    ) -> Result<(), CorexError> {
        self.record_span(
            mi,
            start,
            off,
            flags,
            data.len() as u64,
            is_all_zero(data),
            false,
        )
    }
    fn success_pagemap_zero(
        &mut self,
        mi: usize,
        start: u64,
        off: u64,
        flags: u32,
        len: u64,
    ) -> Result<(), CorexError> {
        self.record_span(mi, start, off, flags, len, true, true)
    }
    fn gap(
        &mut self,
        mi: usize,
        start: u64,
        len: u64,
        errno: Option<i32>,
        kind: &'static str,
    ) -> Result<(), CorexError> {
        if let Some(last) = self.gaps.last_mut()
            && last.mapping_index == mi
            && last.end == start
            && last.errno == errno
            && last.kind == kind
        {
            last.end += len;
            self.failed_bytes += len;
            return Ok(());
        }
        if self.gaps.len() >= MAX_PHDRS {
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
    pub(super) fn reserved_for(expected_mappings: usize) -> Result<Self, CorexError> {
        Ok(Self {
            coverage: Coverage::reserve_for(expected_mappings)?,
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
    clean: bool,
    pub(super) stats: IoStats,
    pub(super) known_zero_ranges: Vec<(usize, usize)>,
}
unsafe impl Send for Snapshot {}
impl Snapshot {
    pub(super) fn allocate(capacity: usize) -> Result<Self, CorexError> {
        if capacity == 0 || capacity > isize::MAX as usize {
            return Err(CorexError::InvalidData("invalid snapshot capacity".into()));
        }
        let stats = IoStats::reserved_for(256)?;
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
            clean: true,
            stats,
            known_zero_ranges: Vec::new(),
        })
    }
    pub(super) fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    fn record_known_zero(&mut self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let end = start + len;
        if let Some(last) = self.known_zero_ranges.last_mut()
            && last.1 == start
        {
            last.1 = end;
            return;
        }
        self.known_zero_ranges.push((start, end));
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.capacity);
        }
    }
}
pub(super) trait CoreSink: Write + Seek {
    fn write_zero_bytes(&mut self, len: u64) -> std::io::Result<()>;
    fn throttle(
        &mut self,
        _c: Option<&crate::engine::CancellationToken>,
        _alive: &dyn Fn() -> Result<(), CorexError>,
    ) -> Result<(), CorexError> {
        Ok(())
    }
}
pub(super) struct SparseWriter<'a> {
    pub file: &'a mut File,
    rate: crate::rate_limit::RateLimiter,
    pending_written: usize,
}
impl<'a> SparseWriter<'a> {
    pub fn new(file: &'a mut File, mbps: Option<u64>) -> Self {
        Self {
            file,
            rate: crate::rate_limit::RateLimiter::new(mbps),
            pending_written: 0,
        }
    }
}
impl Write for SparseWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() < SPARSE_PAGE_BYTES {
            let n = self.file.write(data)?;
            self.pending_written += n;
            return Ok(n);
        }
        let mut pos = 0usize;
        while pos + SPARSE_PAGE_BYTES <= data.len() {
            if is_all_zero(&data[pos..pos + SPARSE_PAGE_BYTES]) {
                let mut hole_end = pos + SPARSE_PAGE_BYTES;
                while hole_end + SPARSE_PAGE_BYTES <= data.len()
                    && is_all_zero(&data[hole_end..hole_end + SPARSE_PAGE_BYTES])
                {
                    hole_end += SPARSE_PAGE_BYTES;
                }
                if let Err(e) = self.file.seek(SeekFrom::Current((hole_end - pos) as i64)) {
                    return if pos > 0 { Ok(pos) } else { Err(e) };
                }
                pos = hole_end;
            } else {
                let mut data_end = pos + SPARSE_PAGE_BYTES;
                while data_end + SPARSE_PAGE_BYTES <= data.len()
                    && !is_all_zero(&data[data_end..data_end + SPARSE_PAGE_BYTES])
                {
                    data_end += SPARSE_PAGE_BYTES;
                }
                match self.file.write(&data[pos..data_end]) {
                    Ok(0) => {
                        return if pos > 0 { Ok(pos) } else { Ok(0) };
                    }
                    Ok(n) => {
                        pos += n;
                        self.pending_written += n;
                        if n < data_end - (pos - n) {
                            return Ok(pos);
                        }
                    }
                    Err(e) => return if pos > 0 { Ok(pos) } else { Err(e) },
                }
            }
        }
        if pos < data.len() {
            match self.file.write(&data[pos..]) {
                Ok(n) => {
                    pos += n;
                    self.pending_written += n;
                }
                Err(e) => return if pos > 0 { Ok(pos) } else { Err(e) },
            }
        }
        Ok(pos)
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
impl CoreSink for SparseWriter<'_> {
    fn throttle(
        &mut self,
        c: Option<&crate::engine::CancellationToken>,
        alive: &dyn Fn() -> Result<(), CorexError>,
    ) -> Result<(), CorexError> {
        let n = std::mem::take(&mut self.pending_written);
        if !self.rate.account(n, || {
            c.is_some_and(crate::engine::CancellationToken::is_cancelled) || alive().is_err()
        }) {
            check_cancelled(c)?;
            alive()?;
            return Err(CorexError::Cancelled);
        }
        Ok(())
    }
    fn write_zero_bytes(&mut self, len: u64) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Current(len as i64)).map(|_| ())
    }
}
impl CoreSink for std::io::Cursor<Vec<u8>> {
    fn write_zero_bytes(&mut self, len: u64) -> std::io::Result<()> {
        let pos = self.position() as usize;
        let target = pos.checked_add(len as usize).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "cursor zero overflow")
        })?;
        if self.get_ref().len() < target {
            self.get_mut().resize(target, 0);
        }
        self.set_position(target as u64);
        Ok(())
    }
}
impl<T: CoreSink + ?Sized> CoreSink for &mut T {
    fn throttle(
        &mut self,
        c: Option<&crate::engine::CancellationToken>,
        alive: &dyn Fn() -> Result<(), CorexError>,
    ) -> Result<(), CorexError> {
        (**self).throttle(c, alive)
    }
    fn write_zero_bytes(&mut self, len: u64) -> std::io::Result<()> {
        (**self).write_zero_bytes(len)
    }
}
struct Layout {
    phdr_slots: usize,
    shoff: u64,
    note: u64,
    first: u64,
    end: u64,
}
fn layout(process: &ProcessInfo, notes: &[u8]) -> Result<Layout, CorexError> {
    let dumped: Vec<_> = process.mappings.iter().filter(|m| m.should_dump).collect();
    let mapped_bytes = dumped
        .iter()
        .try_fold(0u64, |sum, m| sum.checked_add(m.end - m.start))
        .ok_or_else(|| CorexError::InvalidData("mapping size overflow".into()))?;
    let phdr_slots = phdr_capacity_for(dumped.len(), mapped_bytes)?;
    validate_mappings(process, &dumped)?;
    let shoff = ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE * (phdr_slots as u64);
    let note = shoff + ELF_SECTION_HEADER_SIZE;
    let first = align_page(
        note.checked_add(notes.len() as u64)
            .ok_or_else(|| CorexError::InvalidData("ELF note size overflow".into()))?,
        process.page_size,
    )?;
    let end = dumped.iter().try_fold(first, |n, m| {
        n.checked_add(mapping_size(m)?)
            .ok_or_else(|| CorexError::InvalidData("ELF payload size overflow".into()))
    })?;
    Ok(Layout {
        phdr_slots,
        shoff,
        note,
        first,
        end,
    })
}

pub(super) fn pagemap_eligible(m: &Mapping, page: u64) -> bool {
    !m.is_file_backed
        && !m.is_shared
        && m.flags != 0
        && !m.vm_flags_disallow_pagemap_skip
        && m.rss_bytes.is_some()
        && m.start.is_multiple_of(page)
        && m.end.is_multiple_of(page)
        && (m.path.is_empty()
            || m.path == "[heap]"
            || m.path == "[stack]"
            || m.path.starts_with("[stack:")
            || m.path.starts_with("[anon:"))
}

enum PagemapStep {
    ZeroRun(u64),
    ReadRun(u64),
}

fn next_pagemap_step(
    pagemap: &File,
    va: u64,
    end: u64,
    page: u64,
    max_read: u64,
) -> Option<PagemapStep> {
    if !va.is_multiple_of(page) || va >= end {
        return None;
    }
    let remaining_pages = ((end - va) / page) as usize;
    if remaining_pages == 0 {
        return None;
    }
    let inspect_pages = remaining_pages.min(2048);
    let mut raw = vec![0u8; inspect_pages * 8];
    pagemap
        .read_exact_at(&mut raw, (va / page).checked_mul(8)?)
        .ok()?;
    let first = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    let first_zero = (first & PM_NONZERO_BITS) == 0;
    let mut count = 1usize;
    for chunk in raw[8..].as_chunks::<8>().0 {
        let val = u64::from_le_bytes(*chunk);
        let is_zero = (val & PM_NONZERO_BITS) == 0;
        if is_zero != first_zero {
            break;
        }
        count += 1;
        if !first_zero && (count as u64 * page) >= max_read {
            break;
        }
    }
    let bytes = (count as u64).checked_mul(page)?;
    Some(if first_zero {
        PagemapStep::ZeroRun(bytes)
    } else {
        PagemapStep::ReadRun(bytes.min(max_read))
    })
}
enum ReadChunk<'a> {
    Data(&'a [u8]),
    Zero(u64),
}

// Retrying a failed large read at the current page distinguishes a bad page
// from readable pages later in the same VMA. Only successful bytes reach sink.
#[allow(clippy::too_many_arguments)]
fn read_mapping<FSink>(
    memory: &File,
    pagemap: Option<&File>,
    m: &Mapping,
    mi: usize,
    offset: u64,
    page: u64,
    buffer: &mut [u8],
    stats: &mut IoStats,
    cancellation: Option<&crate::engine::CancellationToken>,
    alive: &dyn Fn() -> Result<(), CorexError>,
    mut on_chunk: FSink,
) -> Result<(), CorexError>
where
    FSink: FnMut(u64, ReadChunk<'_>) -> Result<(), CorexError>,
{
    let use_pagemap = pagemap.filter(|_| pagemap_eligible(m, page));
    let mut address = m.start;
    while address < m.end {
        check_cancelled(cancellation)?;
        alive()?;
        let mut requested = ((m.end - address) as usize).min(buffer.len());
        if let Some(pm) = use_pagemap
            && let Some(step) = next_pagemap_step(pm, address, m.end, page, buffer.len() as u64)
        {
            match step {
                PagemapStep::ZeroRun(skip_len) => {
                    let off = offset + address - m.start;
                    stats
                        .coverage
                        .success_pagemap_zero(mi, address, off, m.flags, skip_len)?;
                    on_chunk(off, ReadChunk::Zero(skip_len))?;
                    address += skip_len;
                    continue;
                }
                PagemapStep::ReadRun(read_len) => {
                    requested = (read_len as usize).min(requested);
                }
            }
        }
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
                    on_chunk(off, ReadChunk::Data(&buffer[..n]))?;
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
) -> Result<(Vec<u8>, Option<[u8; ELF_SECTION_HEADER_SIZE as usize]>), CorexError> {
    if c.spans.len() + 1 > l.phdr_slots {
        return Err(CorexError::InvalidData(
            "captured-run count exceeds reserved ELF header slots".into(),
        ));
    }
    let (e_phnum, ext_phnum) = checked_phnum_fields(c.spans.len())?;
    let mut bytes = Vec::with_capacity(64 + 56 * (c.spans.len() + 1));
    let mut ehdr = elf_header(e_phnum);
    if ext_phnum.is_some() {
        put_u64(&mut ehdr, 40, l.shoff);
        put_u16(&mut ehdr, 58, ELF_SECTION_HEADER_SIZE as u16);
        put_u16(&mut ehdr, 60, 1);
    }
    bytes.extend_from_slice(&ehdr);
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
    let shdr = ext_phnum.map(|real_phnum| {
        let mut s = [0_u8; ELF_SECTION_HEADER_SIZE as usize];
        // ELF64 SHT_NULL section header carrying extended phnum in sh_info (offset 44).
        put_u32(&mut s, 44, real_phnum);
        s
    });
    Ok((bytes, shdr))
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
    snapshot_reserved(process, notes, c, memory, None, reader, alive, None)
}
#[allow(clippy::too_many_arguments)]
pub(super) fn snapshot_reserved(
    process: &ProcessInfo,
    notes: &[u8],
    c: Option<&crate::engine::CancellationToken>,
    memory: &File,
    pagemap: Option<&File>,
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
    let was_clean = std::mem::replace(&mut s.clean, false);
    s.len = len;
    s.known_zero_ranges.clear();
    s.stats.coverage = Coverage::reserve_for(process.mappings.len())?;
    s.stats.coverage.init_from_process(process);
    let data = unsafe { std::slice::from_raw_parts_mut(s.ptr, s.len) };
    data[l.note as usize..l.note as usize + notes.len()].copy_from_slice(notes);
    // Read directly into a bounded scratch slice of the reserved snapshot;
    // unpopulated private-anonymous pages are skipped via pagemap without faulting.
    let mut offset = l.first;
    let result = (|| {
        for (mi, m) in process
            .mappings
            .iter()
            .enumerate()
            .filter(|(_, m)| m.should_dump)
        {
            let size = mapping_size(m)? as usize;
            let use_pagemap = pagemap.filter(|_| pagemap_eligible(m, process.page_size));
            let mut cursor = 0usize;
            while cursor < size {
                check_cancelled(c)?;
                alive()?;
                let mut n = (size - cursor).min(MEMORY_CHUNK_SIZE);
                if let Some(pm) = use_pagemap
                    && let Some(step) = next_pagemap_step(
                        pm,
                        m.start + cursor as u64,
                        m.end,
                        process.page_size,
                        MEMORY_CHUNK_SIZE as u64,
                    )
                {
                    match step {
                        PagemapStep::ZeroRun(skip_len) => {
                            let at = m.start + cursor as u64;
                            let file_off = offset + cursor as u64;
                            s.stats
                                .coverage
                                .success_pagemap_zero(mi, at, file_off, m.flags, skip_len)?;
                            s.record_known_zero(file_off as usize, skip_len as usize);
                            cursor += skip_len as usize;
                            continue;
                        }
                        PagemapStep::ReadRun(read_len) => {
                            n = (read_len as usize).min(n);
                        }
                    }
                }
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
    match result {
        Ok(()) => {}
        Err(CorexError::TimedOut) if s.stats.coverage.captured_bytes > 0 => {
            s.stats.coverage.stop_timeout_reached = true;
        }
        Err(e) => return Err(e.with_coverage(s.stats.coverage.json())),
    }
    let (head, shdr) = headers(process, notes, &l, &s.stats.coverage)?;
    data[..head.len()].copy_from_slice(&head);
    if let Some(shdr) = shdr {
        data[l.shoff as usize..l.shoff as usize + shdr.len()].copy_from_slice(&shdr);
    }
    if was_clean {
        let hdr_zero_start = head.len().next_multiple_of(SPARSE_PAGE_BYTES);
        let raw_zero_end = if shdr.is_some() {
            l.shoff as usize
        } else {
            l.note as usize
        };
        let hdr_zero_end = raw_zero_end / SPARSE_PAGE_BYTES * SPARSE_PAGE_BYTES;
        if hdr_zero_start < hdr_zero_end {
            if s.known_zero_ranges
                .first()
                .is_some_and(|first| first.0 == hdr_zero_end)
            {
                s.known_zero_ranges[0].0 = hdr_zero_start;
            } else {
                s.known_zero_ranges
                    .insert(0, (hdr_zero_start, hdr_zero_end));
            }
        }
    }
    Ok(s)
}
#[cfg(test)]
pub(super) fn write<W: CoreSink>(
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
        None,
        &|| Ok(()),
        &mut vec![0; MEMORY_CHUNK_SIZE],
        None,
    )
    .map(|_| ())
}
#[allow(clippy::too_many_arguments)]
pub(super) fn write_reserved<W: CoreSink>(
    path: &Path,
    mut output: W,
    process: &ProcessInfo,
    notes: &[u8],
    c: Option<&crate::engine::CancellationToken>,
    memory: &File,
    pagemap: Option<&File>,
    alive: &dyn Fn() -> Result<(), CorexError>,
    buffer: &mut [u8],
    stats: Option<IoStats>,
) -> Result<IoStats, CorexError> {
    let l = layout(process, notes)?;
    let mut stats = match stats {
        Some(s) => s,
        None => IoStats::reserved_for(process.mappings.len())?,
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
            let read_result = read_mapping(
                memory,
                pagemap,
                m,
                mi,
                offset,
                process.page_size,
                buffer,
                &mut stats,
                c,
                alive,
                |off, chunk| {
                    let begin = crate::now();
                    let result = match chunk {
                        ReadChunk::Data(bytes) => output
                            .seek(SeekFrom::Start(off))
                            .and_then(|_| output.write_all(bytes))
                            .map_err(|e| CorexError::io("write captured bytes", &display, e)),
                        ReadChunk::Zero(len) => output
                            .seek(SeekFrom::Start(off))
                            .and_then(|_| output.write_zero_bytes(len))
                            .map_err(|e| CorexError::io("skip zero bytes", &display, e)),
                    };
                    writes += 1;
                    elapsed += crate::now() - begin;
                    result?;
                    output.throttle(c, alive)
                },
            );
            stats.write_calls += writes;
            stats.write_ns += elapsed;
            read_result?;
            offset += mapping_size(m)?;
        }
        Ok::<_, CorexError>(())
    })();
    match result {
        Ok(()) => {}
        Err(CorexError::TimedOut) if stats.coverage.captured_bytes > 0 => {
            stats.coverage.stop_timeout_reached = true;
        }
        Err(e) => return Err(e.with_coverage(stats.coverage.json())),
    }
    let (head, shdr) = headers(process, notes, &l, &stats.coverage)?;
    output
        .seek(SeekFrom::Start(0))
        .and_then(|_| output.write_all(&head))
        .and_then(|_| {
            if let Some(shdr) = shdr {
                output.seek(SeekFrom::Start(l.shoff))?;
                output.write_all(&shdr)?;
            }
            output.seek(SeekFrom::Start(l.end))
        })
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
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: None,
                swap_bytes: None,
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
    fn extended_program_headers_emit_pn_xnum_in_both_writers() {
        assert_eq!(checked_phnum_fields(0).unwrap(), (1, None));
        assert_eq!(checked_phnum_fields(65533).unwrap(), (65534, None));
        assert_eq!(checked_phnum_fields(65534).unwrap(), (0xffff, Some(65535)));
        assert!(checked_phnum_fields(usize::MAX).is_err());

        let mut process = process();
        process.mappings = (0..65534u64)
            .map(|idx| Mapping {
                start: 0x10000 + idx * 4096,
                end: 0x10000 + (idx + 1) * 4096,
                flags: 4,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: true,
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: None,
                swap_bytes: None,
                path: String::new(),
                raw_path: Vec::new(),
            })
            .collect();
        let zero = File::open("/dev/zero").unwrap();
        let l = layout(&process, &[]).unwrap();

        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::memfd_create(c"pn_xnum_test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut out_file = unsafe { File::from_raw_fd(fd) };
        let mut scratch = vec![0; MEMORY_CHUNK_SIZE];
        let stats = write_reserved(
            Path::new("memfd_pn_xnum"),
            SparseWriter::new(&mut out_file, None),
            &process,
            &[],
            None,
            &zero,
            None,
            &|| Ok(()),
            &mut scratch,
            None,
        )
        .unwrap();
        assert_eq!(stats.coverage.spans.len(), 65534);
        let mut ehdr = [0u8; 64];
        out_file.read_exact_at(&mut ehdr, 0).unwrap();
        assert_eq!(
            u16::from_le_bytes(ehdr[56..58].try_into().unwrap()),
            PN_XNUM
        );
        assert_eq!(
            u64::from_le_bytes(ehdr[40..48].try_into().unwrap()),
            l.shoff
        );
        assert_eq!(u16::from_le_bytes(ehdr[58..60].try_into().unwrap()), 64);
        assert_eq!(u16::from_le_bytes(ehdr[60..62].try_into().unwrap()), 1);
        let mut shdr = [0u8; 64];
        out_file.read_exact_at(&mut shdr, l.shoff).unwrap();
        assert_eq!(u32::from_le_bytes(shdr[44..48].try_into().unwrap()), 65535);
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
            let mut writer = SparseWriter::new(&mut file, None);
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
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: Some((length * page) as u64),
                swap_bytes: Some(0),
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
            SparseWriter::new(&mut out, None),
            &process,
            &[],
            None,
            &memory,
            None,
            &|| Ok(()),
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
        for size in [
            0usize, 1, 7, 15, 16, 31, 32, 63, 64, 127, 128, 255, 4096, 65536,
        ] {
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
        let snap = snapshot_reserved(
            &process,
            b"note-bytes",
            None,
            &mem,
            None,
            "proc_mem",
            &|| Ok(()),
            None,
        )
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
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: None,
                swap_bytes: None,
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
                exclusion_reason: Some("uncommitted_anonymous_prot_none"),
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: Some(0),
                swap_bytes: Some(0),
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
                exclusion_reason: Some("kernel_special_unreadable"),
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: None,
                swap_bytes: None,
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

    #[test]
    fn stream_writer_enforces_alive_callback_between_chunks() {
        let process = tests::process();
        let zero = File::open("/dev/zero").unwrap();
        let mut out = std::io::Cursor::new(Vec::new());
        let mut scratch = vec![0; MEMORY_CHUNK_SIZE];
        let err = write_reserved(
            Path::new("stream_alive"),
            &mut out,
            &process,
            &[],
            None,
            &zero,
            None,
            &|| Err(CorexError::InvalidData("target exited mid-stream".into())),
            &mut scratch,
            None,
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("target exited mid-stream"));
    }

    #[test]
    fn stop_timeout_after_first_mapping_preserves_valid_partial_elf_in_both_modes() {
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
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: None,
                swap_bytes: None,
                path: "first".into(),
                raw_path: b"first".to_vec(),
            },
            Mapping {
                start: 0x2000,
                end: 0x2000 + page,
                flags: 4,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: true,
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: None,
                swap_bytes: None,
                path: "second".into(),
                raw_path: b"second".to_vec(),
            },
        ];
        let zero = File::open("/dev/zero").unwrap();
        let token = crate::engine::CancellationToken::with_timeout(60_000);
        let calls = std::cell::Cell::new(0usize);
        let alive = || {
            let n = calls.get() + 1;
            calls.set(n);
            if n == 2 {
                // Simulate stop_token timing out after the first mapping is captured.
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            Ok(())
        };
        let stop_token = token.capped(40);
        let snap = snapshot_reserved(
            &process,
            b"NOTE",
            Some(&stop_token),
            &zero,
            None,
            "proc_mem",
            &alive,
            None,
        )
        .unwrap();
        assert!(snap.stats.coverage.stop_timeout_reached);
        assert!(!snap.stats.coverage.complete());
        assert_eq!(snap.stats.coverage.captured_bytes, page);
        assert_eq!(snap.stats.coverage.unattempted_bytes(), page);
        assert_eq!(snap.stats.coverage.spans.len(), 1);
        assert_eq!(&snap.bytes()[..4], b"\x7fELF");
        assert!(read_elf(snap.bytes(), 0x1000).is_some());
        assert!(read_elf(snap.bytes(), 0x2000).is_none());

        let mut out = std::io::Cursor::new(Vec::new());
        let mut scratch = vec![0; MEMORY_CHUNK_SIZE];
        let calls_stream = std::cell::Cell::new(0usize);
        let stop_stream = token.capped(40);
        let alive_stream = || {
            let n = calls_stream.get() + 1;
            calls_stream.set(n);
            if n == 2 {
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            Ok(())
        };
        let stats = write_reserved(
            Path::new("stream_partial"),
            &mut out,
            &process,
            b"NOTE",
            Some(&stop_stream),
            &zero,
            None,
            &alive_stream,
            &mut scratch,
            None,
        )
        .unwrap();
        assert!(stats.coverage.stop_timeout_reached);
        assert!(!stats.coverage.complete());
        assert_eq!(stats.coverage.captured_bytes, page);
        assert_eq!(stats.coverage.unattempted_bytes(), page);
        assert_eq!(
            out.get_ref().as_slice(),
            &snap.bytes()[..out.get_ref().len()]
        );
        assert!(read_elf(out.get_ref(), 0x1000).is_some());
        assert!(read_elf(out.get_ref(), 0x2000).is_none());
    }

    #[test]
    fn pagemap_read_skip_matches_full_read_byte_for_byte() {
        let pages = 32usize;
        let page = 4096usize;
        let alloc_pattern = || {
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    pages * page,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(base, libc::MAP_FAILED);
            unsafe {
                libc::madvise(base, pages * page, libc::MADV_NOHUGEPAGE);
            }
            let region = Region(base, pages * page);
            let slice = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), pages * page) };
            // Fault pages 0, 5..7, 19, 31 with mixed nonzero/zero patterns; leave rest un-faulted demand zero.
            slice[0..128].fill(0xaa);
            slice[5 * page..6 * page].fill(0); // Faulted zero page (present=1, all zero!)
            slice[6 * page..7 * page].fill(0x5a);
            slice[19 * page + 4090] = 0x7e;
            slice[31 * page + 10] = 0xff;
            let mut process = tests::process();
            process.pid = std::process::id() as i32;
            process.mappings = vec![Mapping {
                start: base as u64,
                end: (base as u64) + (pages * page) as u64,
                flags: 6,
                offset: 0,
                is_shared: false,
                is_file_backed: false,
                should_dump: true,
                exclusion_reason: None,
                vm_flags_disallow_pagemap_skip: false,
                rss_bytes: Some((5 * page) as u64),
                swap_bytes: Some(0),
                path: String::new(),
                raw_path: Vec::new(),
            }];
            (region, process)
        };

        let memory = File::open("/proc/self/mem").unwrap();
        let pagemap = File::open("/proc/self/pagemap").unwrap();

        // Run both Read-Skip passes BEFORE full /proc/self/mem reads so GUP does not pre-fault demand-zero PTEs.
        let (_region, proc_info) = alloc_pattern();
        let skipped = snapshot_reserved(
            &proc_info,
            b"N",
            None,
            &memory,
            Some(&pagemap),
            "proc_mem",
            &|| Ok(()),
            None,
        )
        .unwrap();
        let mut stream_skip = std::io::Cursor::new(Vec::new());
        let mut scratch = vec![0u8; MEMORY_CHUNK_SIZE];
        let stream_skip_stats = write_reserved(
            Path::new("skip"),
            &mut stream_skip,
            &proc_info,
            b"N",
            None,
            &memory,
            Some(&pagemap),
            &|| Ok(()),
            &mut scratch,
            None,
        )
        .unwrap();
        let full = snapshot_reserved(
            &proc_info,
            b"N",
            None,
            &memory,
            None,
            "proc_mem",
            &|| Ok(()),
            None,
        )
        .unwrap();
        let mut stream_full = std::io::Cursor::new(Vec::new());
        write_reserved(
            Path::new("full"),
            &mut stream_full,
            &proc_info,
            b"N",
            None,
            &memory,
            None,
            &|| Ok(()),
            &mut scratch,
            None,
        )
        .unwrap();

        assert_eq!(skipped.bytes(), full.bytes());
        assert!(skipped.stats.coverage.pagemap_skipped_bytes >= 26 * page as u64);
        assert!(stream_skip_stats.coverage.pagemap_skipped_bytes >= 26 * page as u64);
        assert_eq!(skipped.stats.coverage.captured_bytes, (pages * page) as u64);
        assert_eq!(skipped.stats.coverage.spans.len(), 1);
        assert_eq!(
            serde_json::to_string(&skipped.stats.coverage.spans).unwrap(),
            serde_json::to_string(&full.stats.coverage.spans).unwrap()
        );
        assert!(!skipped.known_zero_ranges.is_empty());
        assert_eq!(stream_skip.get_ref(), stream_full.get_ref());
        assert_eq!(stream_skip.get_ref().as_slice(), full.bytes());
    }

    #[test]
    fn sparse_writer_handles_4kib_interleaved_pages_and_trailing_hole() {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;
        let fd = unsafe { libc::memfd_create(c"sparse_4k".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut out = unsafe { File::from_raw_fd(fd) };
        let mut payload = vec![0u8; 4096 * 6];
        payload[0..4096].fill(0x11);
        // pages 1..2 zero (8 KiB)
        payload[4096 * 3..4096 * 4].fill(0x22);
        // pages 4..6 trailing zero hole (8 KiB)
        {
            let mut writer = SparseWriter::new(&mut out, None);
            writer.write_all(&payload).unwrap();
            writer.flush().unwrap();
        }
        let meta = out.metadata().unwrap();
        assert_eq!(meta.len(), payload.len() as u64);
        assert!(
            meta.blocks() * 512 < meta.len(),
            "expected 4 KiB sparse holes to allocate fewer disk blocks ({} vs {})",
            meta.blocks() * 512,
            meta.len()
        );
        let mut actual = vec![0u8; payload.len()];
        out.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(actual, payload);
    }
    #[test]
    fn streaming_rate_limit_counts_data_but_not_sparse_holes() {
        let path = std::env::temp_dir().join(format!(
            "poddump_rate_{}_{}",
            std::process::id(),
            crate::now()
        ));
        let mut file = File::create(&path).unwrap();
        let mut writer = SparseWriter::new(&mut file, Some(1));
        let start = std::time::Instant::now();
        writer.write_all(&vec![0x5a; 350_000]).unwrap();
        assert_eq!(writer.pending_written, 350_000);
        writer.throttle(None, &|| Ok(())).unwrap();
        assert!(start.elapsed() >= std::time::Duration::from_millis(350));
        writer.write_zero_bytes(16 * 1024 * 1024).unwrap();
        assert_eq!(writer.pending_written, 0);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn fragmented_mapping_retains_more_than_old_header_slack() {
        let mut process = tests::process();
        process.mappings.truncate(1);
        process.mappings[0].start = 0x1000;
        process.mappings[0].end = 0x1000 + 1024 * 4096;
        let l = layout(&process, &[]).unwrap();
        let mut coverage = Coverage::reserve().unwrap();
        for i in 0..512 {
            coverage.spans.push(ReadSpan {
                start: 0x1000 + i * 8192,
                end: 0x2000 + i * 8192,
                file_offset: l.first + i * 8192,
                flags: 4,
                mapping_index: 0,
            });
        }
        assert!(headers(&process, &[], &l, &coverage).is_ok());
    }

    #[test]
    fn pagemap_guard_and_userfaultfd_markers_are_not_zero_runs() {
        let path = std::env::temp_dir().join(format!(
            "poddump_pm_marker_{}_{}",
            std::process::id(),
            crate::now()
        ));
        for marker in [1u64 << 58, 1u64 << 57, 1u64 << 62, 1u64 << 63] {
            std::fs::write(&path, marker.to_le_bytes()).unwrap();
            let file = File::open(&path).unwrap();
            assert!(matches!(
                next_pagemap_step(&file, 0, 4096, 4096, 4096),
                Some(PagemapStep::ReadRun(4096))
            ));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_registers_unused_header_slack_in_known_zero_ranges() {
        let page = 4096usize;
        let pages = 1024usize;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                pages * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let _region = Region(base, pages * page);
        let addr = base as u64;
        let mut proc_info = tests::process();
        proc_info.mappings = vec![Mapping {
            start: addr,
            end: addr + (pages * page) as u64,
            flags: 6,
            offset: 0,
            is_shared: false,
            is_file_backed: false,
            should_dump: true,
            exclusion_reason: None,
            vm_flags_disallow_pagemap_skip: true,
            rss_bytes: Some(0),
            swap_bytes: Some(0),
            path: String::new(),
            raw_path: Vec::new(),
        }];
        let l = layout(&proc_info, b"N").unwrap();
        assert!(l.note > (page * 2) as u64);
        let memory = File::open("/proc/self/mem").unwrap();
        let snap = snapshot_reserved(
            &proc_info,
            b"N",
            None,
            &memory,
            None,
            "proc_mem",
            &|| Ok(()),
            None,
        )
        .unwrap();
        let (hz_start, hz_end) = snap.known_zero_ranges[0];
        assert_eq!(hz_start, page);
        assert!(hz_end <= l.note as usize);
        assert!(hz_end > hz_start);
        assert!(snap.bytes()[hz_start..hz_end].iter().all(|&b| b == 0));
    }

    #[test]
    fn userfaultfd_missing_vma_is_never_skipped_as_zero_run() {
        let mut proc_um = tests::process();
        proc_um.mappings.truncate(1);
        proc_um.mappings[0].flags = 6;
        proc_um.mappings[0].is_shared = false;
        proc_um.mappings[0].is_file_backed = false;
        proc_um.mappings[0].vm_flags_disallow_pagemap_skip = true;
        assert!(!pagemap_eligible(&proc_um.mappings[0], proc_um.page_size));

        #[repr(C)]
        struct UffdioApi {
            api: u64,
            features: u64,
            ioctls: u64,
        }
        #[repr(C)]
        struct UffdioRange {
            start: u64,
            len: u64,
        }
        #[repr(C)]
        struct UffdioRegister {
            range: UffdioRange,
            mode: u64,
            ioctls: u64,
        }
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::close(fds[0]);
                // Create userfaultfd inside the forked child so ctx->mm binds to the child's mm.
                let mut uffd = libc::syscall(libc::SYS_userfaultfd, 1) as i32;
                if uffd < 0 {
                    uffd = libc::syscall(libc::SYS_userfaultfd, 0) as i32;
                }
                if uffd < 0 {
                    let zero = 0u64.to_le_bytes();
                    let _ = libc::write(fds[1], zero.as_ptr().cast(), 8);
                    libc::close(fds[1]);
                    libc::_exit(0);
                }
                let mut api = UffdioApi {
                    api: 0xaa,
                    features: 0,
                    ioctls: 0,
                };
                if libc::syscall(
                    libc::SYS_ioctl,
                    uffd,
                    0xc018_aa3f_u32 as libc::c_ulong,
                    &mut api,
                ) != 0
                {
                    libc::_exit(11);
                }
                let addr = libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if addr == libc::MAP_FAILED {
                    libc::_exit(12);
                }
                let mut reg = UffdioRegister {
                    range: UffdioRange {
                        start: addr as u64,
                        len: 4096,
                    },
                    mode: 1,
                    ioctls: 0,
                };
                if libc::syscall(
                    libc::SYS_ioctl,
                    uffd,
                    0xc020_aa00_u32 as libc::c_ulong,
                    &mut reg,
                ) != 0
                {
                    libc::_exit(13);
                }
                let val = (addr as u64).to_le_bytes();
                if libc::write(fds[1], val.as_ptr().cast(), 8) != 8 {
                    libc::_exit(14);
                }
                libc::close(fds[1]);
                loop {
                    libc::pause();
                }
            }
        }
        struct ChildGuard {
            pid: i32,
        }
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                    let mut st = 0;
                    libc::waitpid(self.pid, &mut st, 0);
                }
            }
        }
        let _guard = ChildGuard { pid };
        unsafe { libc::close(fds[1]) };
        let mut buf = [0u8; 8];
        let mut read_total = 0usize;
        while read_total < 8 {
            let n = unsafe {
                libc::read(
                    fds[0],
                    buf[read_total..].as_mut_ptr().cast(),
                    8 - read_total,
                )
            };
            assert!(n > 0, "child failed setting up userfaultfd");
            read_total += n as usize;
        }
        unsafe { libc::close(fds[0]) };
        let uaddr = u64::from_le_bytes(buf);
        if uaddr == 0 {
            return;
        }
        let _attached = crate::corex::ptrace::AttachedThreads::attach_process(pid, None).unwrap();
        let mut proc_info = crate::corex::procfs::read_process(pid).unwrap();
        proc_info
            .mappings
            .retain(|m| m.start <= uaddr && m.end >= uaddr + 4096);
        assert_eq!(proc_info.mappings.len(), 1);
        assert!(proc_info.mappings[0].should_dump);
        assert!(proc_info.mappings[0].vm_flags_disallow_pagemap_skip);
        let memory = File::open(format!("/proc/{pid}/mem")).unwrap();
        let pagemap = File::open(format!("/proc/{pid}/pagemap")).unwrap();
        let snap = snapshot_reserved(
            &proc_info,
            b"N",
            None,
            &memory,
            Some(&pagemap),
            "proc_mem",
            &|| Ok(()),
            None,
        )
        .unwrap();
        assert_eq!(snap.stats.coverage.pagemap_skipped_bytes, 0);
        assert!(snap.stats.coverage.failed_bytes >= 4096);
    }
}
