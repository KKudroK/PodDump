use super::{CorexError, ProcessInfo, ThreadState};

const NOTE_ALIGNMENT: usize = 4;
const NT_PRSTATUS: u32 = 1;
const NT_FPREGSET: u32 = 2;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;

const NT_FILE: u32 = 0x4649_4c45;
const NT_ARM_PAC_MASK: u32 = 0x406;
const PRSTATUS_REG_OFFSET: usize = 112;

pub(super) fn build(process: &ProcessInfo, threads: &[ThreadState]) -> Result<Vec<u8>, CorexError> {
    if !process.page_size.is_power_of_two() {
        return Err(CorexError::InvalidData("invalid note page size".into()));
    }
    if !process.auxv.is_empty()
        && (process.auxv.len() % 16 != 0
            || process
                .auxv
                .chunks_exact(16)
                .position(|entry| entry[..8] == [0; 8])
                != Some(process.auxv.len() / 16 - 1))
    {
        return Err(CorexError::InvalidData(
            "invalid or truncated 64-bit auxiliary vector".into(),
        ));
    }
    let mut notes = Vec::with_capacity(256 * 1024);
    for thread in threads {
        if thread.fp_regs.len() != fp_regset_size() {
            return Err(CorexError::InvalidData(
                "invalid FP register note length".into(),
            ));
        }
        if !thread.xstate.is_empty() && thread.xstate.len() < 576 {
            return Err(CorexError::InvalidData(
                "truncated XSAVE register note".into(),
            ));
        }
        append(
            &mut notes,
            b"CORE",
            NT_PRSTATUS,
            &prstatus(process, thread)?,
        )?;
        append(&mut notes, b"CORE", NT_FPREGSET, &thread.fp_regs)?;
        if !thread.xstate.is_empty() {
            append(&mut notes, b"LINUX", 0x202, &thread.xstate)?;
        }
        if let Some(mask) = thread.pac_mask {
            let mut descriptor = Vec::with_capacity(16);
            descriptor.extend_from_slice(&mask[0].to_ne_bytes());
            descriptor.extend_from_slice(&mask[1].to_ne_bytes());
            append(&mut notes, b"LINUX", NT_ARM_PAC_MASK, &descriptor)?;
        }
    }
    append(&mut notes, b"CORE", NT_PRPSINFO, &prpsinfo(process))?;
    // NT_SIGINFO omitted: this collector did not observe a fatal signal.
    if !process.auxv.is_empty() {
        append(&mut notes, b"CORE", NT_AUXV, &process.auxv)?;
    }
    if let Some(file_note) = file_note(process)? {
        append(&mut notes, b"CORE", NT_FILE, &file_note)?;
    }
    Ok(notes)
}

fn append(
    notes: &mut Vec<u8>,
    owner: &[u8],
    note_type: u32,
    descriptor: &[u8],
) -> Result<(), CorexError> {
    let name_size = owner
        .len()
        .checked_add(1)
        .ok_or_else(|| CorexError::InvalidData("ELF note owner is too large".into()))?;
    let descriptor_size = u32::try_from(descriptor.len())
        .map_err(|_| CorexError::InvalidData("ELF note descriptor is too large".into()))?;
    let name_size = u32::try_from(name_size)
        .map_err(|_| CorexError::InvalidData("ELF note owner is too large".into()))?;
    notes.extend_from_slice(&name_size.to_ne_bytes());
    notes.extend_from_slice(&descriptor_size.to_ne_bytes());
    notes.extend_from_slice(&note_type.to_ne_bytes());
    notes.extend_from_slice(owner);
    notes.push(0);
    pad(notes, NOTE_ALIGNMENT);
    notes.extend_from_slice(descriptor);
    pad(notes, NOTE_ALIGNMENT);
    Ok(())
}

fn prstatus(process: &ProcessInfo, thread: &ThreadState) -> Result<Vec<u8>, CorexError> {
    if thread.gp_regs.len() != gp_regset_size() {
        return Err(CorexError::InvalidData(format!(
            "thread {} GP register size is {}, expected {}",
            thread.tid,
            thread.gp_regs.len(),
            gp_regset_size()
        )));
    }
    let mut status = vec![0_u8; prstatus_size()];
    write_i32(&mut status, 32, thread.tid);
    write_i32(&mut status, 36, process.ppid);
    write_i32(&mut status, 40, process.pgrp);
    write_i32(&mut status, 44, process.sid);
    status[PRSTATUS_REG_OFFSET..PRSTATUS_REG_OFFSET + thread.gp_regs.len()]
        .copy_from_slice(&thread.gp_regs);
    // Linux marks pr_fpvalid when it emits the corresponding FP regset note.
    write_i32(
        &mut status,
        PRSTATUS_REG_OFFSET + thread.gp_regs.len(),
        i32::from(!thread.fp_regs.is_empty()),
    );
    Ok(status)
}

fn prpsinfo(process: &ProcessInfo) -> Vec<u8> {
    let mut info = vec![0_u8; 136];
    // Synthetic Core: no instantaneous scheduler state was observed here.
    info[0] = 0xff;
    info[1] = b'?';
    write_u32(&mut info, 16, process.uid);
    write_u32(&mut info, 20, process.gid);
    write_i32(&mut info, 24, process.pid);
    write_i32(&mut info, 28, process.ppid);
    write_i32(&mut info, 32, process.pgrp);
    write_i32(&mut info, 36, process.sid);
    copy_truncated(&mut info[40..56], &process.comm);

    let mut arguments: Vec<_> = process
        .cmdline
        .iter()
        .map(|byte| if *byte == 0 { b' ' } else { *byte })
        .take(79)
        .collect();
    while arguments.last() == Some(&b' ') {
        arguments.pop();
    }
    copy_truncated(&mut info[56..136], &arguments);
    info
}

fn file_note(process: &ProcessInfo) -> Result<Option<Vec<u8>>, CorexError> {
    let mappings: Vec<_> = process
        .mappings
        .iter()
        .filter(|mapping| mapping.raw_path.starts_with(b"/"))
        .collect();
    if mappings.is_empty() {
        return Ok(None);
    }
    if !process.page_size.is_power_of_two()
        || mappings.iter().any(|m| {
            m.start >= m.end || m.offset % process.page_size != 0 || m.raw_path.contains(&0)
        })
    {
        return Err(CorexError::InvalidData("invalid file mapping note".into()));
    }
    let names_size = mappings
        .iter()
        .try_fold(0usize, |total, mapping| {
            mapping
                .raw_path
                .len()
                .checked_add(1)
                .and_then(|size| total.checked_add(size))
        })
        .ok_or_else(|| CorexError::InvalidData("file note size overflow".into()))?;
    let capacity = mappings
        .len()
        .checked_mul(24)
        .and_then(|size| size.checked_add(16))
        .and_then(|size| size.checked_add(names_size))
        .filter(|size| u32::try_from(*size).is_ok())
        .ok_or_else(|| CorexError::InvalidData("file note exceeds ELF descriptor size".into()))?;
    let mut descriptor = Vec::with_capacity(capacity);
    descriptor.extend_from_slice(&(mappings.len() as u64).to_ne_bytes());
    descriptor.extend_from_slice(&process.page_size.to_ne_bytes());
    for mapping in &mappings {
        descriptor.extend_from_slice(&mapping.start.to_ne_bytes());
        descriptor.extend_from_slice(&mapping.end.to_ne_bytes());
        descriptor.extend_from_slice(&(mapping.offset / process.page_size).to_ne_bytes());
    }
    for mapping in mappings {
        descriptor.extend_from_slice(&mapping.raw_path);
        descriptor.push(0);
    }
    Ok(Some(descriptor))
}

fn copy_truncated(output: &mut [u8], value: &[u8]) {
    let length = value.len().min(output.len().saturating_sub(1));
    output[..length].copy_from_slice(&value[..length]);
}

fn write_i32(output: &mut [u8], offset: usize, value: i32) {
    output[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn pad(output: &mut Vec<u8>, alignment: usize) {
    let aligned = output.len().div_ceil(alignment) * alignment;
    output.resize(aligned, 0);
}

#[cfg(target_arch = "x86_64")]
const fn fp_regset_size() -> usize {
    512
}
#[cfg(target_arch = "aarch64")]
const fn fp_regset_size() -> usize {
    528
}
#[cfg(not(target_endian = "little"))]
compile_error!("PodDump ELF output currently supports little-endian targets only");

#[cfg(target_arch = "aarch64")]
const fn gp_regset_size() -> usize {
    272
}

#[cfg(target_arch = "aarch64")]
const fn prstatus_size() -> usize {
    392
}

#[cfg(target_arch = "x86_64")]
const fn gp_regset_size() -> usize {
    216
}

#[cfg(target_arch = "x86_64")]
const fn prstatus_size() -> usize {
    336
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corex::Mapping;

    fn process() -> ProcessInfo {
        ProcessInfo {
            pid: 42,
            ppid: 1,
            pgrp: 42,
            sid: 42,
            uid: 1000,
            gid: 1000,
            comm: b"worker".to_vec(),
            exe: "/tmp/worker".into(),
            cmdline: b"worker\0run\0".to_vec(),
            mappings: vec![Mapping {
                start: 0x1000,
                end: 0x2000,
                flags: 5,
                offset: 0,
                is_shared: false,
                is_file_backed: true,
                should_dump: true,
                path: "/tmp/worker".into(),
                raw_path: b"/tmp/worker".to_vec(),
            }],
            auxv: vec![0; 16],
            coredump_filter: 0x33,
            page_size: 4096,
            tids: vec![42],
        }
    }

    #[test]
    fn malformed_auxv_and_register_notes_are_rejected() {
        let valid = ThreadState {
            tid: 42,
            gp_regs: vec![0; gp_regset_size()],
            fp_regs: vec![0; fp_regset_size()],
            pac_mask: None,
            xstate: vec![],
        };
        let mut p = process();
        p.auxv = vec![1; 15];
        assert!(build(&p, &[valid.clone()]).is_err());
        p.auxv = vec![1; 16];
        assert!(build(&p, &[valid.clone()]).is_err());
        let mut thread = valid.clone();
        thread.fp_regs.pop();
        assert!(build(&process(), &[thread]).is_err());
        let mut thread = valid.clone();
        thread.gp_regs.pop();
        assert!(build(&process(), &[thread]).is_err());
        let mut thread = valid;
        thread.xstate = vec![0; 575];
        assert!(build(&process(), &[thread]).is_err());
        let mut p = process();
        p.page_size = 0;
        assert!(build(&p, &[]).is_err());
        let mut p = process();
        p.mappings[0].offset = 1;
        assert!(build(&p, &[]).is_err());
    }

    #[test]
    fn prpsinfo_matches_linux_abi_offsets() {
        let info = prpsinfo(&process());
        assert_eq!(info.len(), 136);
        assert_eq!(&info[24..28], &42_i32.to_ne_bytes());
        assert_eq!(&info[40..46], b"worker");
        assert_eq!(&info[56..66], b"worker run");
    }

    #[test]
    fn file_note_uses_page_offsets() {
        let descriptor = file_note(&process()).unwrap().unwrap();
        assert_eq!(&descriptor[0..8], &1_u64.to_ne_bytes());
        assert_eq!(&descriptor[8..16], &process().page_size.to_ne_bytes());
        assert!(descriptor.ends_with(b"/tmp/worker\0"));
    }

    #[test]
    fn file_note_preserves_distinct_non_utf8_raw_paths() {
        let mut p = process();
        p.mappings = vec![
            Mapping {
                start: 0x1000,
                end: 0x2000,
                flags: 5,
                offset: 0,
                is_shared: false,
                is_file_backed: true,
                should_dump: true,
                path: "/tmp/\u{FFFD}.bin".into(),
                raw_path: b"/tmp/\x80.bin".to_vec(),
            },
            Mapping {
                start: 0x2000,
                end: 0x3000,
                flags: 5,
                offset: 0,
                is_shared: false,
                is_file_backed: true,
                should_dump: true,
                path: "/tmp/\u{FFFD}.bin".into(),
                raw_path: b"/tmp/\x81.bin".to_vec(),
            },
        ];
        let descriptor = file_note(&p).unwrap().unwrap();
        assert_eq!(&descriptor[0..8], &2_u64.to_ne_bytes());
        // Header (16) + 2 mappings * 24 = 64 bytes
        let filenames = &descriptor[64..];
        assert_eq!(filenames, b"/tmp/\x80.bin\0/tmp/\x81.bin\0");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn emitted_fp_note_has_valid_prstatus_flag_and_bounds() {
        let thread = ThreadState {
            tid: 42,
            gp_regs: vec![0x41; 216],
            fp_regs: vec![0x42; 512],
            pac_mask: None,
            xstate: Vec::new(),
        };
        let notes = build(&process(), &[thread]).unwrap();
        let mut at = 0;
        let mut statuses = 0;
        let mut fp_notes = 0;
        while at < notes.len() {
            assert!(at + 12 <= notes.len());
            let namesz = u32::from_ne_bytes(notes[at..at + 4].try_into().unwrap()) as usize;
            let size = u32::from_ne_bytes(notes[at + 4..at + 8].try_into().unwrap()) as usize;
            let kind = u32::from_ne_bytes(notes[at + 8..at + 12].try_into().unwrap());
            let descriptor = at + 12 + namesz.div_ceil(4) * 4;
            assert!(descriptor + size <= notes.len());
            if kind == NT_PRSTATUS {
                assert_eq!(size, 336);
                assert_eq!(
                    i32::from_ne_bytes(
                        notes[descriptor + 328..descriptor + 332]
                            .try_into()
                            .unwrap()
                    ),
                    1
                );
                assert_eq!(&notes[descriptor + 112..descriptor + 328], &[0x41; 216]);
                statuses += 1;
            }
            if kind == NT_FPREGSET {
                assert_eq!(size, 512);
                assert_eq!(&notes[descriptor..descriptor + size], &[0x42; 512]);
                fp_notes += 1;
            }
            at = descriptor + size.div_ceil(4) * 4;
        }
        assert_eq!(at, notes.len());
        assert_eq!((statuses, fp_notes), (1, 1));
    }
}
