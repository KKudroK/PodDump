use super::{CorexError, Mapping, ProcessInfo};
use std::fs::File;
use std::io::Write;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

const ELF_HEADER_SIZE: u64 = 64;
const PROGRAM_HEADER_SIZE: u64 = 56;
const MEMORY_CHUNK_SIZE: usize = 1024 * 1024;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;

fn checked_phnum(selected_mappings: usize) -> Result<u16, CorexError> {
    selected_mappings
        .checked_add(1)
        .filter(|count| *count < 0xffff)
        .and_then(|count| u16::try_from(count).ok())
        .ok_or_else(|| CorexError::InvalidData(
            "extended ELF program-header counts are unsupported (maximum 65533 selected mappings)".into(),
        ))
}

fn validate_mappings(process: &ProcessInfo, mappings: &[&Mapping]) -> Result<(), CorexError> {
    if !process.page_size.is_power_of_two() {
        return Err(CorexError::InvalidData("invalid ELF page alignment".into()));
    }
    let mut previous_end = 0;
    for mapping in mappings {
        if mapping.start >= mapping.end
            || mapping.start < previous_end
            || mapping.start % process.page_size != 0
            || mapping.end % process.page_size != 0
            || mapping.flags & !7 != 0
        {
            return Err(CorexError::InvalidData(
                "invalid, overlapping, unaligned or unsupported ELF mapping".into(),
            ));
        }
        previous_end = mapping.end;
    }
    Ok(())
}

/// Anonymous mmap avoids the extra per-chunk buffer-to-Vec copy in the upstream
/// writer. The mapping is zero-initialized by Linux and bounded by admission in
/// dump(); only successfully read bytes become a completed core.
pub(super) struct Snapshot {
    ptr: *mut u8,
    len: usize,
}
impl Snapshot {
    pub(super) fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}
pub(super) struct SparseWriter<'a> {
    pub file: &'a mut File,
}
impl Write for SparseWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() >= 4096 && data.iter().all(|b| *b == 0) {
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
pub(super) fn snapshot(
    process: &ProcessInfo,
    notes: &[u8],
    cancellation: Option<&crate::engine::CancellationToken>,
    memory: &File,
    reader: &str,
    alive: &dyn Fn() -> Result<(), CorexError>,
) -> Result<Snapshot, CorexError> {
    let dumped: Vec<_> = process.mappings.iter().filter(|m| m.should_dump).collect();
    let count = checked_phnum(dumped.len())?;
    validate_mappings(process, &dumped)?;
    let note_at = ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE * u64::from(count);
    let first = align_page(
        note_at
            .checked_add(notes.len() as u64)
            .ok_or_else(|| CorexError::InvalidData("ELF note size overflow".into()))?,
        process.page_size,
    )?;
    let len = dumped.iter().try_fold(first, |n, m| {
        n.checked_add(mapping_size(m)?)
            .ok_or_else(|| CorexError::InvalidData("snapshot size overflow".into()))
    })?;
    let len =
        usize::try_from(len).map_err(|_| CorexError::InvalidData("snapshot too large".into()))?;
    if len > isize::MAX as usize {
        return Err(CorexError::InvalidData(
            "snapshot exceeds addressable slice length".into(),
        ));
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
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
    let snapshot = Snapshot {
        ptr: ptr.cast(),
        len,
    };
    let data = unsafe { std::slice::from_raw_parts_mut(snapshot.ptr, snapshot.len) };
    data[..64].copy_from_slice(&elf_header(count));
    data[64..120].copy_from_slice(&program_header(
        PT_NOTE,
        0,
        note_at,
        0,
        notes.len() as u64,
        0,
        4,
    ));
    data[note_at as usize..note_at as usize + notes.len()].copy_from_slice(notes);

    let mut off = first as usize;
    for (i, m) in dumped.iter().enumerate() {
        let size = mapping_size(m)? as usize;
        data[120 + i * 56..176 + i * 56].copy_from_slice(&program_header(
            PT_LOAD,
            m.flags,
            off as u64,
            m.start,
            size as u64,
            size as u64,
            process.page_size,
        ));
        let mut cursor = 0;
        while cursor < size {
            check_cancelled(cancellation)?;
            let n = (size - cursor).min(2 * 1024 * 1024);
            alive()?;
            let read = if reader == "readv" && m.flags & 4 != 0 {
                let local = libc::iovec {
                    iov_base: data[off + cursor..].as_mut_ptr().cast(),
                    iov_len: n,
                };
                let remote = libc::iovec {
                    iov_base: (m.start + cursor as u64) as *mut libc::c_void,
                    iov_len: n,
                };
                let got = unsafe { libc::process_vm_readv(process.pid, &local, 1, &remote, 1, 0) };
                if got < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(got as usize)
                }
            } else {
                memory.read_at(
                    &mut data[off + cursor..off + cursor + n],
                    m.start + cursor as u64,
                )
            };
            match read {
                Ok(0) => {
                    return Err(CorexError::InvalidData(format!(
                        "zero read at {:#x}",
                        m.start + cursor as u64
                    )));
                }
                Ok(got) => cursor += got,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(CorexError::io(
                        "read process memory",
                        format!(
                            "{:#x}-{:#x}",
                            m.start + cursor as u64,
                            m.start + cursor as u64 + n as u64
                        ),
                        e,
                    ));
                }
            }
        }
        off += size;
    }
    Ok(snapshot)
}

pub(super) fn write<W: Write>(
    path: &Path,
    mut output: W,
    process: &ProcessInfo,
    notes: &[u8],
    cancellation: Option<&crate::engine::CancellationToken>,
    memory: &File,
) -> Result<(), CorexError> {
    let dumped: Vec<_> = process
        .mappings
        .iter()
        .filter(|mapping| mapping.should_dump)
        .collect();
    let program_header_count = checked_phnum(dumped.len())?;
    validate_mappings(process, &dumped)?;
    let note_offset = ELF_HEADER_SIZE
        .checked_add(PROGRAM_HEADER_SIZE * u64::from(program_header_count))
        .ok_or_else(|| CorexError::InvalidData("ELF note offset overflow".into()))?;
    let first_load_offset = align_page(
        note_offset
            .checked_add(notes.len() as u64)
            .ok_or_else(|| CorexError::InvalidData("ELF note size overflow".into()))?,
        process.page_size,
    )?;
    let mut next_offset = first_load_offset;
    let mut load_offsets = Vec::with_capacity(dumped.len());
    for mapping in &dumped {
        let size = mapping_size(mapping)?;
        load_offsets.push(next_offset);
        next_offset = next_offset
            .checked_add(size)
            .ok_or_else(|| CorexError::InvalidData("ELF load offset overflow".into()))?;
    }

    let display_path = path.display().to_string();
    output
        .write_all(&elf_header(program_header_count))
        .map_err(|source| CorexError::io("write", &display_path, source))?;
    output
        .write_all(&program_header(
            PT_NOTE,
            0,
            note_offset,
            0,
            notes.len() as u64,
            0,
            4,
        ))
        .map_err(|source| CorexError::io("write", &display_path, source))?;
    for (mapping, offset) in dumped.iter().zip(&load_offsets) {
        let size = mapping_size(mapping)?;
        output
            .write_all(&program_header(
                PT_LOAD,
                mapping.flags,
                *offset,
                mapping.start,
                size,
                size,
                process.page_size,
            ))
            .map_err(|source| CorexError::io("write", &display_path, source))?;
    }
    output
        .write_all(notes)
        .map_err(|source| CorexError::io("write", &display_path, source))?;
    write_zeroes(
        &mut output,
        usize::try_from(first_load_offset - note_offset - notes.len() as u64)
            .map_err(|_| CorexError::InvalidData("ELF padding is too large".into()))?,
        &display_path,
    )?;

    let mut buffer = vec![0_u8; MEMORY_CHUNK_SIZE];
    for mapping in dumped {
        check_cancelled(cancellation)?;
        write_mapping(
            &mut output,
            &memory,
            mapping,
            &display_path,
            cancellation,
            &mut buffer,
        )?;
    }
    output
        .flush()
        .map_err(|source| CorexError::io("flush", &display_path, source))
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

fn write_mapping<W: Write>(
    output: &mut W,
    memory: &File,
    mapping: &Mapping,
    output_path: &str,
    cancellation: Option<&crate::engine::CancellationToken>,
    buffer: &mut [u8],
) -> Result<(), CorexError> {
    let mut address = mapping.start;
    let mut remaining = mapping_size(mapping)?;
    while remaining > 0 {
        check_cancelled(cancellation)?;
        let requested = usize::try_from(remaining.min(MEMORY_CHUNK_SIZE as u64)).unwrap();
        let read = match memory.read_at(&mut buffer[..requested], address) {
            Ok(0) => {
                return Err(CorexError::InvalidData(format!(
                    "zero-byte memory read at {address:#x}"
                )));
            }
            Ok(read) => read,
            Err(error) => {
                return Err(CorexError::io(
                    "read process memory for",
                    format!("{:#x}-{:#x}", mapping.start, mapping.end),
                    error,
                ));
            }
        };
        output
            .write_all(&buffer[..read])
            .map_err(|source| CorexError::io("write", output_path, source))?;
        address += read as u64;
        remaining -= read as u64;
    }
    Ok(())
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

fn write_zeroes<W: Write>(output: &mut W, mut size: usize, path: &str) -> Result<(), CorexError> {
    let zeroes = [0_u8; 4096];
    while size > 0 {
        let count = size.min(zeroes.len());
        output
            .write_all(&zeroes[..count])
            .map_err(|source| CorexError::io("write", path, source))?;
        size -= count;
    }
    Ok(())
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

    fn process() -> ProcessInfo {
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
        let mut output = Vec::new();
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
        assert!(output.is_empty());
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
            let mut output = Vec::new();
            assert!(write(Path::new("unused"), &mut output, &p, &[], None, &memory).is_err());
            assert!(output.is_empty());
            assert!(snapshot(&p, &[], None, &memory, "proc_mem", &|| Ok(())).is_err());
        }
        assert!(align_page(u64::MAX, 4096).is_err());
    }

    #[test]
    fn stream_and_snapshot_layouts_match_and_short_reads_fail() {
        let process = process();
        let zero = File::open("/dev/zero").unwrap();
        let mut output = Vec::new();
        write(Path::new("unused"), &mut output, &process, &[], None, &zero).unwrap();
        let buffered = snapshot(&process, &[], None, &zero, "proc_mem", &|| Ok(())).unwrap();
        assert_eq!(output, buffered.bytes());
        assert_eq!(output.len(), 8192);
        assert_eq!(u16::from_le_bytes(output[56..58].try_into().unwrap()), 2);
        assert_eq!(
            u64::from_le_bytes(output[128..136].try_into().unwrap()),
            4096
        );
        let empty = File::open("/dev/null").unwrap();
        assert!(write(Path::new("unused"), Vec::new(), &process, &[], None, &empty).is_err());
        assert!(snapshot(&process, &[], None, &empty, "proc_mem", &|| Ok(())).is_err());
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
