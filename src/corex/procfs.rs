use super::{CorexError, Mapping, ProcessInfo};
use std::fs::{self, File};
use std::io::Read;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

pub(super) fn enumerate_threads(pid: i32) -> Result<Vec<i32>, CorexError> {
    let path = format!("/proc/{pid}/task");
    let entries = fs::read_dir(&path).map_err(|source| CorexError::io("open", &path, source))?;
    let mut tids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CorexError::io("read", &path, source))?;
        if let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|tid| *tid > 0)
        {
            tids.push(tid);
        }
    }
    tids.sort_unstable_by_key(|tid| (*tid != pid, *tid));
    if tids.is_empty() {
        return Err(CorexError::InvalidData(format!(
            "no threads found for process {pid}"
        )));
    }
    Ok(tids)
}

pub(super) fn read_process(pid: i32) -> Result<ProcessInfo, CorexError> {
    let (ppid, pgrp, sid, uid, gid) = read_status(pid)?;
    Ok(ProcessInfo {
        pid,
        ppid,
        pgrp,
        sid,
        uid,
        gid,
        comm: read_trimmed(format!("/proc/{pid}/comm"), 16)?,
        exe: fs::read_link(format!("/proc/{pid}/exe"))
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|e| CorexError::io("read executable symlink", pid.to_string(), e))?,
        cmdline: read_limited(format!("/proc/{pid}/cmdline"), 131_072)?,
        mappings: read_maps(pid)?,
        auxv: read_limited(format!("/proc/{pid}/auxv"), 65_536)?,
        coredump_filter: read_coredump_filter(pid),
        page_size: page_size()?,
        tids: enumerate_threads(pid)?,
    })
}

fn is_anon_prot_none(m: &Mapping) -> bool {
    m.flags == 0
        && !m.is_file_backed
        && !m.is_shared
        && !matches!(
            m.path.as_str(),
            "[vvar]" | "[vdso]" | "[vsyscall]" | "[vectors]"
        )
        && !m.path.starts_with("[vvar")
}

fn read_maps(pid: i32) -> Result<Vec<Mapping>, CorexError> {
    let path = format!("/proc/{pid}/maps");
    let raw = fs::read(&path).map_err(|source| CorexError::io("read", &path, source))?;
    let mut mappings = Vec::new();
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        mappings.push(
            parse_mapping(line).ok_or_else(|| {
                CorexError::InvalidData("invalid mapping line in proc maps".into())
            })?,
        );
    }
    disambiguate_map_files(pid, &mut mappings);
    if let Ok(smaps_raw) = fs::read(format!("/proc/{pid}/smaps")) {
        let smaps_contents = String::from_utf8_lossy(&smaps_raw);
        apply_smaps_inspection(&mut mappings, &smaps_contents);
    }
    Ok(mappings)
}

fn disambiguate_map_files(pid: i32, mappings: &mut [Mapping]) {
    use std::os::unix::ffi::OsStrExt;
    for m in mappings.iter_mut() {
        if m.is_file_backed && m.raw_path.starts_with(b"/") {
            let link_path = format!("/proc/{pid}/map_files/{:x}-{:x}", m.start, m.end);
            if let Ok(target) = fs::read_link(&link_path) {
                let raw = target.as_os_str().as_bytes().to_vec();
                if !raw.is_empty() {
                    m.path = String::from_utf8_lossy(&raw).into_owned();
                    m.raw_path = raw;
                }
            }
        }
    }
}

fn apply_smaps_inspection(mappings: &mut [Mapping], smaps: &str) {
    let mut current_range: Option<(u64, u64)> = None;
    let mut current_rss: Option<u64> = None;
    let mut current_swap: Option<u64> = None;
    let mut current_device_or_pfn = false;
    let mut current_disallow_pagemap_skip = true;
    let mut map_cursor = 0usize;

    let mut apply = |range: Option<(u64, u64)>,
                     rss_kb: Option<u64>,
                     swap_kb: Option<u64>,
                     device_or_pfn: bool,
                     disallow_pagemap_skip: bool| {
        let Some((start, end)) = range else {
            return;
        };
        let pos = mappings[map_cursor..]
            .iter()
            .position(|m| m.start == start && m.end == end)
            .map(|offset| map_cursor + offset)
            .or_else(|| {
                mappings
                    .iter()
                    .position(|m| m.start == start && m.end == end)
            });
        let Some(idx) = pos else {
            return;
        };
        if idx >= map_cursor {
            map_cursor = idx + 1;
        }
        let m = &mut mappings[idx];
        m.rss_bytes = rss_kb.map(|kb| kb.saturating_mul(1024));
        m.swap_bytes = swap_kb.map(|kb| kb.saturating_mul(1024));
        m.vm_flags_disallow_pagemap_skip = disallow_pagemap_skip;
        if !m.should_dump {
            return;
        }
        if device_or_pfn {
            m.should_dump = false;
            m.exclusion_reason = Some("device_io_or_pfnmap");
        } else if is_anon_prot_none(m)
            && !disallow_pagemap_skip
            && rss_kb == Some(0)
            && swap_kb == Some(0)
        {
            m.should_dump = false;
            m.exclusion_reason = Some("uncommitted_anonymous_prot_none");
        }
    };

    for line in smaps.lines() {
        if line.contains('-')
            && line.as_bytes().first().is_some_and(u8::is_ascii_hexdigit)
            && let Some((start_str, rest)) = line.split_once('-')
            && let Some((end_str, _)) = rest.split_once(' ')
            && let (Ok(start), Ok(end)) = (
                u64::from_str_radix(start_str, 16),
                u64::from_str_radix(end_str, 16),
            )
        {
            apply(
                current_range,
                current_rss,
                current_swap,
                current_device_or_pfn,
                current_disallow_pagemap_skip,
            );
            current_range = Some((start, end));
            current_rss = None;
            current_swap = None;
            current_device_or_pfn = false;
            current_disallow_pagemap_skip = true;
            continue;
        }
        if current_range.is_some() {
            if let Some(rest) = line.strip_prefix("Rss:") {
                current_rss = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok());
            } else if let Some(rest) = line.strip_prefix("Swap:") {
                current_swap = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok());
            } else if let Some(rest) = line.strip_prefix("VmFlags:") {
                current_disallow_pagemap_skip = rest.trim().is_empty();
                for flag in rest.split_whitespace() {
                    if matches!(flag, "io" | "pf") {
                        current_device_or_pfn = true;
                        current_disallow_pagemap_skip = true;
                    } else if matches!(flag, "ht" | "mm" | "sh" | "ms" | "um" | "uw" | "ui") {
                        // Do NOT treat VM_DONTDUMP ("dd") as an exclusion: anti-forensic
                        // MADV_DONTDUMP regions remain within all_mapped_proc_mem policy.
                        current_disallow_pagemap_skip = true;
                    }
                }
            }
        }
    }
    apply(
        current_range,
        current_rss,
        current_swap,
        current_device_or_pfn,
        current_disallow_pagemap_skip,
    );
}

fn parse_mapping(line: &[u8]) -> Option<Mapping> {
    let mut rest = line;
    let mut fields: Vec<&[u8]> = Vec::with_capacity(5);
    for _ in 0..5 {
        while let Some((&b, tail)) = rest.split_first() {
            if b == b' ' || b == b'\t' {
                rest = tail;
            } else {
                break;
            }
        }
        let end = rest
            .iter()
            .position(|&b| b == b' ' || b == b'\t')
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    let range_str = std::str::from_utf8(fields[0]).ok()?;
    let mut range = range_str.splitn(2, '-');
    let start = u64::from_str_radix(range.next()?, 16).ok()?;
    let end = u64::from_str_radix(range.next()?, 16).ok()?;
    let permissions = fields[1];
    if start >= end
        || permissions.len() != 4
        || !matches!(permissions[0], b'r' | b'-')
        || !matches!(permissions[1], b'w' | b'-')
        || !matches!(permissions[2], b'x' | b'-')
        || !matches!(permissions[3], b'p' | b's')
    {
        return None;
    }
    let offset_str = std::str::from_utf8(fields[2]).ok()?;
    let offset = u64::from_str_radix(offset_str, 16).ok()?;
    let inode_str = std::str::from_utf8(fields[4]).ok()?;
    let inode = inode_str.parse::<u64>().ok()?;
    while let Some((&b, tail)) = rest.split_first() {
        if b == b' ' || b == b'\t' {
            rest = tail;
        } else {
            break;
        }
    }
    let raw_path = rest.to_vec();
    let path = String::from_utf8_lossy(&raw_path).into_owned();
    let mut flags = 0;
    if permissions.first() == Some(&b'r') {
        flags |= PF_R;
    }
    if permissions.get(1) == Some(&b'w') {
        flags |= PF_W;
    }
    if permissions.get(2) == Some(&b'x') {
        flags |= PF_X;
    }
    let kernel_special = is_kernel_special_unreadable(&path);
    let should_dump = !kernel_special;
    Some(Mapping {
        start,
        end,
        flags,
        offset,
        is_shared: permissions.get(3) == Some(&b's'),
        is_file_backed: inode > 0,
        should_dump,
        exclusion_reason: kernel_special.then_some("kernel_special_unreadable"),
        vm_flags_disallow_pagemap_skip: true,
        rss_bytes: None,
        swap_bytes: None,
        path,
        raw_path,
    })
}

pub(super) fn is_kernel_special_unreadable(path: &str) -> bool {
    matches!(path, "[vvar]" | "[vsyscall]") || path.starts_with("[vvar")
}

fn read_status(pid: i32) -> Result<(i32, i32, i32, u32, u32), CorexError> {
    let path = format!("/proc/{pid}/status");
    let raw = fs::read(&path).map_err(|source| CorexError::io("read", &path, source))?;
    let contents = String::from_utf8_lossy(&raw);
    for key in ["PPid:", "Uid:", "Gid:", "NSpgid:", "NSsid:"] {
        if !contents.lines().any(|l| {
            l.starts_with(key)
                && l.split_whitespace()
                    .nth(1)
                    .is_some_and(|v| v.parse::<u32>().is_ok())
        }) {
            return Err(CorexError::InvalidData(format!(
                "missing or invalid required status field {key}"
            )));
        }
    }
    let mut ppid = 0;
    let mut pgrp = 0;
    let mut sid = 0;
    let mut uid = 0;
    let mut gid = 0;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("PPid:") => ppid = parse_next(&mut fields),
            Some("Uid:") => uid = parse_next(&mut fields),
            Some("Gid:") => gid = parse_next(&mut fields),
            Some("NSpgid:") => pgrp = parse_next(&mut fields),
            Some("NSsid:") => sid = parse_next(&mut fields),
            _ => {}
        }
    }
    if pgrp == 0 && sid == 0 {
        (pgrp, sid) = read_groups_from_stat(pid).unwrap_or_default();
    }
    Ok((ppid, pgrp, sid, uid, gid))
}

fn parse_next<T: Default + std::str::FromStr>(fields: &mut dyn Iterator<Item = &str>) -> T {
    fields
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or_default()
}

fn read_groups_from_stat(pid: i32) -> Option<(i32, i32)> {
    let stat_raw = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat_raw.iter().rposition(|&b| b == b')')?;
    let tail_bytes = stat_raw.get(rparen + 1..)?;
    let tail = std::str::from_utf8(tail_bytes).ok()?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    Some((fields.get(2)?.parse().ok()?, fields.get(3)?.parse().ok()?))
}

fn read_trimmed(path: String, limit: usize) -> Result<Vec<u8>, CorexError> {
    let mut value = read_limited(path, limit)?;
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        value.pop();
    }
    Ok(value)
}

fn read_limited(path: String, limit: usize) -> Result<Vec<u8>, CorexError> {
    let file = File::open(&path).map_err(|source| CorexError::io("open", &path, source))?;
    let mut value = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut value)
        .map_err(|source| CorexError::io("read", &path, source))?;
    Ok(value)
}

fn read_coredump_filter(pid: i32) -> u32 {
    fs::read_to_string(format!("/proc/{pid}/coredump_filter"))
        .ok()
        .and_then(|value| u32::from_str_radix(value.trim(), 16).ok())
        .unwrap_or(0x33)
}

fn page_size() -> Result<u64, CorexError> {
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value <= 0 {
        Err(CorexError::InvalidData(
            "unable to determine system page size".into(),
        ))
    } else {
        Ok(value as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_mapping() {
        let mapping = parse_mapping(
            b"ffff800080000000-ffff800080021000 r-xp 00000000 08:01 42 /usr/bin/test app",
        )
        .unwrap();
        assert_eq!(mapping.start, 0xffff800080000000);
        assert_eq!(mapping.end, 0xffff800080021000);
        assert_eq!(mapping.flags, PF_R | PF_X);
        assert!(mapping.is_file_backed);
        assert!(!mapping.is_shared);
        assert_eq!(mapping.path, "/usr/bin/test app");
        assert_eq!(mapping.raw_path, b"/usr/bin/test app");
    }

    #[test]
    fn mapping_paths_preserve_spacing_and_bad_permissions_fail() {
        let mapping = parse_mapping(b"1000-2000 r--p 00000000 08:01 42    /tmp/a  b \r").unwrap();
        assert_eq!(mapping.path, "/tmp/a  b \r");
        assert_eq!(mapping.raw_path, b"/tmp/a  b \r");
        assert!(parse_mapping(b"1000-2000 rwqp 00000000 08:01 42 /tmp/x").is_none());
        assert!(parse_mapping(b"2000-1000 r--p 00000000 08:01 42 /tmp/x").is_none());
        assert!(parse_mapping(b"1000-1000 r--p 00000000 08:01 42 /tmp/x").is_none());
    }

    #[test]
    fn uncommitted_prot_none_of_any_size_requires_zero_rss_and_zero_swap() {
        let mut mappings = vec![
            parse_mapping(
                b"000000c000000000-000000c004000000 ---p 00000000 00:00 0 [anon: Go: heap reservation]",
            )
            .unwrap(),
            parse_mapping(b"000000c004000000-000000c008000000 ---p 00000000 00:00 0").unwrap(),
            parse_mapping(b"000000c008000000-000000c00c000000 ---p 00000000 00:00 0").unwrap(),
            parse_mapping(b"00007f0000000000-00007f0000001000 ---p 00000000 00:00 0").unwrap(),
        ];
        let smaps = "\
000000c000000000-000000c004000000 ---p 00000000 00:00 0 [anon: Go: heap reservation]\n\
Size:              65536 kB\n\
Rss:                   0 kB\n\
Swap:                  0 kB\n\
VmFlags: mr mw me ac sd\n\
000000c004000000-000000c008000000 ---p 00000000 00:00 0 \n\
Size:              65536 kB\n\
Rss:                   0 kB\n\
Swap:                  4 kB\n\
VmFlags: mr mw me ac sd\n\
000000c008000000-000000c00c000000 ---p 00000000 00:00 0 \n\
Size:              65536 kB\n\
Rss:                   4 kB\n\
Swap:                  0 kB\n\
VmFlags: mr mw me ac sd\n\
00007f0000000000-00007f0000001000 ---p 00000000 00:00 0 \n\
Size:                  4 kB\n\
Rss:                   0 kB\n\
Swap:                  0 kB\n\
VmFlags: mr mw me ac sd\n";
        apply_smaps_inspection(&mut mappings, smaps);
        assert!(!mappings[0].should_dump);
        assert_eq!(
            mappings[0].exclusion_reason,
            Some("uncommitted_anonymous_prot_none")
        );
        assert!(mappings[1].should_dump);
        assert!(mappings[2].should_dump);
        assert!(!mappings[3].should_dump);
        assert_eq!(
            mappings[3].exclusion_reason,
            Some("uncommitted_anonymous_prot_none")
        );
    }

    #[test]
    fn smaps_excludes_vm_io_and_pfnmap_while_retaining_vm_dontdump() {
        let mut mappings = vec![
            parse_mapping(b"1000-2000 rw-s 00000000 00:06 10 /dev/zero").unwrap(),
            parse_mapping(b"3000-4000 rw-p 00000000 00:00 0").unwrap(),
            parse_mapping(b"5000-6000 rw-p 00000000 00:00 0").unwrap(),
            parse_mapping(b"7000-8000 rw-p 00000000 00:00 0").unwrap(),
        ];
        let smaps = "\
00001000-00002000 rw-s 00000000 00:06 10 /dev/zero\n\
Rss:                  4 kB\n\
Swap:                 0 kB\n\
VmFlags: rd wr sh mr mw me io\n\
00003000-00004000 rw-p 00000000 00:00 0\n\
Rss:                  4 kB\n\
Swap:                 0 kB\n\
VmFlags: rd wr mr mw me pf\n\
00005000-00006000 rw-p 00000000 00:00 0\n\
Rss:                  4 kB\n\
Swap:                 0 kB\n\
VmFlags: rd wr mr mw me dd ac\n\
00007000-00008000 rw-p 00000000 00:00 0\n\
Rss:                  4 kB\n\
Swap:                 0 kB\n\
VmFlags: rd wr mr mw me ht\n";
        apply_smaps_inspection(&mut mappings, smaps);
        assert!(!mappings[0].should_dump);
        assert_eq!(mappings[0].exclusion_reason, Some("device_io_or_pfnmap"));
        assert!(!mappings[1].should_dump);
        assert_eq!(mappings[1].exclusion_reason, Some("device_io_or_pfnmap"));
        assert!(
            mappings[2].should_dump,
            "MADV_DONTDUMP must remain captured"
        );
        assert!(!mappings[2].vm_flags_disallow_pagemap_skip);
        assert!(mappings[3].should_dump);
        assert!(
            mappings[3].vm_flags_disallow_pagemap_skip,
            "VM_HUGETLB must disallow 4KiB pagemap skip"
        );
    }

    #[test]
    fn kernel_special_unreadable_mappings_are_not_selected_for_dump() {
        let vvar =
            parse_mapping(b"7fff7fbfb000-7fff7fbfd000 r--p 00000000 00:00 0 [vvar]").unwrap();
        assert!(!vvar.should_dump);
        let vsyscall =
            parse_mapping(b"ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]")
                .unwrap();
        assert!(!vsyscall.should_dump);
        let vdso =
            parse_mapping(b"7fff7fbfe000-7fff7fc00000 r-xp 00000000 00:00 0 [vdso]").unwrap();
        assert!(vdso.should_dump);
        let heap = parse_mapping(b"000055a123400000-000055a123421000 rw-p 00000000 00:00 0 [heap]")
            .unwrap();
        assert!(heap.should_dump);
    }

    #[test]
    fn parses_mapping_with_non_utf8_path() {
        let mut raw =
            b"ffff800080000000-ffff800080021000 r-xp 00000000 08:01 42 /usr/bin/test_".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        let mapping = parse_mapping(&raw).unwrap();
        assert_eq!(mapping.start, 0xffff800080000000);
        assert_eq!(mapping.end, 0xffff800080021000);
        assert!(mapping.path.contains("test_"));
        assert_eq!(mapping.raw_path, b"/usr/bin/test_\xff\xfe\xfd");
    }

    #[test]
    fn disambiguate_map_files_safe_fallback_on_inaccessible() {
        let mapping =
            parse_mapping(b"1000-2000 r--p 00000000 08:01 42 /tmp/test\\012file").unwrap();
        let mut mappings = vec![mapping];
        disambiguate_map_files(9999999, &mut mappings);
        assert_eq!(mappings[0].raw_path, b"/tmp/test\\012file");
    }
    #[test]
    fn missing_vmflags_and_special_prot_none_never_authorize_zero_omission() {
        for flags in [
            "",
            "VmFlags: ht mr mw me\n",
            "VmFlags: sh mr mw me\n",
            "VmFlags: ms mr mw me\n",
            "VmFlags: mr mw me um ac sd\n",
            "VmFlags: mr mw me uw ac sd\n",
            "VmFlags: mr mw me ui ac sd\n",
        ] {
            let mut maps = vec![parse_mapping(b"1000-2000 ---p 00000000 00:00 0").unwrap()];
            apply_smaps_inspection(
                &mut maps,
                &format!("1000-2000 ---p 00000000 00:00 0\nRss: 0 kB\nSwap: 0 kB\n{flags}"),
            );
            assert!(maps[0].should_dump, "flags `{flags}` excluded");
            assert!(
                maps[0].vm_flags_disallow_pagemap_skip,
                "flags `{flags}` allowed pagemap skip"
            );
        }
        let mut shared_maps = vec![parse_mapping(b"1000-2000 ---s 00000000 00:00 0").unwrap()];
        apply_smaps_inspection(
            &mut shared_maps,
            "1000-2000 ---s 00000000 00:00 0\nRss: 0 kB\nSwap: 0 kB\nVmFlags: mr mw me ac sd\n",
        );
        assert!(shared_maps[0].should_dump);
    }

    #[test]
    fn userfaultfd_rw_vma_disallows_pagemap_skip() {
        for flags in [
            "VmFlags: rd wr mr mw me um ac sd\n",
            "VmFlags: rd wr mr mw me uw ac sd\n",
            "VmFlags: rd wr mr mw me ui ac sd\n",
            "VmFlags: rd wr mr mw me ms ac sd\n",
        ] {
            let mut maps = vec![parse_mapping(b"1000-2000 rw-p 00000000 00:00 0").unwrap()];
            apply_smaps_inspection(
                &mut maps,
                &format!("1000-2000 rw-p 00000000 00:00 0\nRss: 0 kB\nSwap: 0 kB\n{flags}"),
            );
            assert!(maps[0].should_dump);
            assert!(
                maps[0].vm_flags_disallow_pagemap_skip,
                "flags `{flags}` allowed pagemap skip"
            );
        }
    }
}
