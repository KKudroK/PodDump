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
        cmdline: read_limited(format!("/proc/{pid}/cmdline"), 4096)?,
        mappings: read_maps(pid)?,
        auxv: read_limited(format!("/proc/{pid}/auxv"), 4096)?,
        coredump_filter: read_coredump_filter(pid),
        page_size: page_size()?,
        tids: enumerate_threads(pid)?,
    })
}

const UNCOMMITTED_PROT_NONE_MIN_BYTES: u64 = 16 * 1024 * 1024;

fn is_large_anon_prot_none(m: &Mapping) -> bool {
    m.flags == 0
        && !m.is_file_backed
        && !matches!(
            m.path.as_str(),
            "[vvar]" | "[vdso]" | "[vsyscall]" | "[vectors]"
        )
        && m.end.saturating_sub(m.start) >= UNCOMMITTED_PROT_NONE_MIN_BYTES
}

fn read_maps(pid: i32) -> Result<Vec<Mapping>, CorexError> {
    let path = format!("/proc/{pid}/maps");
    let contents =
        fs::read_to_string(&path).map_err(|source| CorexError::io("read", &path, source))?;
    let mut mappings = Vec::new();
    for line in contents.split_terminator('\n') {
        mappings.push(
            parse_mapping(line)
                .ok_or_else(|| CorexError::InvalidData(format!("invalid mapping line: {line}")))?,
        );
    }
    if mappings.iter().any(is_large_anon_prot_none) {
        if let Ok(smaps_contents) = fs::read_to_string(format!("/proc/{pid}/smaps")) {
            filter_uncommitted_prot_none(&mut mappings, &smaps_contents);
        }
    }
    Ok(mappings)
}

fn filter_uncommitted_prot_none(mappings: &mut [Mapping], smaps: &str) {
    let mut current_range: Option<(u64, u64)> = None;
    let mut current_rss: Option<u64> = None;
    let mut current_swap: Option<u64> = None;
    let mut apply = |range: Option<(u64, u64)>, rss: Option<u64>, swap: Option<u64>| {
        if let (Some((start, end)), Some(0), Some(0)) = (range, rss, swap) {
            if let Some(m) = mappings
                .iter_mut()
                .find(|m| m.start == start && m.end == end && is_large_anon_prot_none(m))
            {
                m.should_dump = false;
            }
        }
    };
    for line in smaps.lines() {
        if line.contains('-') && line.as_bytes().first().is_some_and(u8::is_ascii_hexdigit) {
            if let Some((start_str, rest)) = line.split_once('-') {
                if let Some((end_str, _)) = rest.split_once(' ') {
                    if let (Ok(start), Ok(end)) = (
                        u64::from_str_radix(start_str, 16),
                        u64::from_str_radix(end_str, 16),
                    ) {
                        apply(current_range, current_rss, current_swap);
                        current_range = Some((start, end));
                        current_rss = None;
                        current_swap = None;
                        continue;
                    }
                }
            }
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
            }
        }
    }
    apply(current_range, current_rss, current_swap);
}

fn parse_mapping(line: &str) -> Option<Mapping> {
    let mut rest = line;
    let mut fields = Vec::with_capacity(5);
    for _ in 0..5 {
        rest = rest.trim_start_matches([' ', '\t']);
        let end = rest.find([' ', '\t']).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    let mut range = fields[0].splitn(2, '-');
    let start = u64::from_str_radix(range.next()?, 16).ok()?;
    let end = u64::from_str_radix(range.next()?, 16).ok()?;
    let permissions = fields[1].as_bytes();
    if start >= end
        || permissions.len() != 4
        || !matches!(permissions[0], b'r' | b'-')
        || !matches!(permissions[1], b'w' | b'-')
        || !matches!(permissions[2], b'x' | b'-')
        || !matches!(permissions[3], b'p' | b's')
    {
        return None;
    }
    let offset = u64::from_str_radix(fields[2], 16).ok()?;
    let inode = fields[4].parse::<u64>().ok()?;
    let path = rest.trim_start_matches([' ', '\t']).to_owned();
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
    let should_dump = !is_kernel_special_unreadable(&path);
    Some(Mapping {
        start,
        end,
        flags,
        offset,
        is_shared: permissions.get(3) == Some(&b's'),
        is_file_backed: inode > 0,
        should_dump,
        path,
    })
}

pub(super) fn is_kernel_special_unreadable(path: &str) -> bool {
    matches!(path, "[vvar]" | "[vsyscall]") || path.starts_with("[vvar")
}

fn read_status(pid: i32) -> Result<(i32, i32, i32, u32, u32), CorexError> {
    let path = format!("/proc/{pid}/status");
    let contents =
        fs::read_to_string(&path).map_err(|source| CorexError::io("read", &path, source))?;
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
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
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
            "ffff800080000000-ffff800080021000 r-xp 00000000 08:01 42 /usr/bin/test app",
        )
        .unwrap();
        assert_eq!(mapping.start, 0xffff800080000000);
        assert_eq!(mapping.end, 0xffff800080021000);
        assert_eq!(mapping.flags, PF_R | PF_X);
        assert!(mapping.is_file_backed);
        assert!(!mapping.is_shared);
        assert_eq!(mapping.path, "/usr/bin/test app");
    }

    #[test]
    fn mapping_paths_preserve_spacing_and_bad_permissions_fail() {
        let mapping = parse_mapping("1000-2000 r--p 00000000 08:01 42    /tmp/a  b \r").unwrap();
        assert_eq!(mapping.path, "/tmp/a  b \r");
        assert!(parse_mapping("1000-2000 rwqp 00000000 08:01 42 /tmp/x").is_none());
        assert!(parse_mapping("2000-1000 r--p 00000000 08:01 42 /tmp/x").is_none());
        assert!(parse_mapping("1000-1000 r--p 00000000 08:01 42 /tmp/x").is_none());
    }

    #[test]
    fn uncommitted_large_prot_none_requires_zero_rss_and_zero_swap() {
        let mut mappings = vec![
            parse_mapping(
                "000000c000000000-000000c004000000 ---p 00000000 00:00 0 [anon: Go: heap reservation]",
            )
            .unwrap(),
            parse_mapping("000000c004000000-000000c008000000 ---p 00000000 00:00 0").unwrap(),
            parse_mapping("000000c008000000-000000c00c000000 ---p 00000000 00:00 0").unwrap(),
            parse_mapping("00007f0000000000-00007f0000001000 ---p 00000000 00:00 0").unwrap(),
        ];
        let smaps = "\
000000c000000000-000000c004000000 ---p 00000000 00:00 0 [anon: Go: heap reservation]\n\
Size:              65536 kB\n\
Rss:                   0 kB\n\
Swap:                  0 kB\n\
000000c004000000-000000c008000000 ---p 00000000 00:00 0 \n\
Size:              65536 kB\n\
Rss:                   0 kB\n\
Swap:                  4 kB\n\
000000c008000000-000000c00c000000 ---p 00000000 00:00 0 \n\
Size:              65536 kB\n\
Rss:                   4 kB\n\
Swap:                  0 kB\n\
00007f0000000000-00007f0000001000 ---p 00000000 00:00 0 \n\
Size:                  4 kB\n\
Rss:                   0 kB\n\
Swap:                  0 kB\n";
        filter_uncommitted_prot_none(&mut mappings, smaps);
        assert!(!mappings[0].should_dump);
        assert!(mappings[1].should_dump);
        assert!(mappings[2].should_dump);
        assert!(mappings[3].should_dump);
    }

    #[test]
    fn kernel_special_unreadable_mappings_are_not_selected_for_dump() {
        let vvar = parse_mapping("7fff7fbfb000-7fff7fbfd000 r--p 00000000 00:00 0 [vvar]").unwrap();
        assert!(!vvar.should_dump);
        let vsyscall = parse_mapping("ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]").unwrap();
        assert!(!vsyscall.should_dump);
        let vdso = parse_mapping("7fff7fbfe000-7fff7fc00000 r-xp 00000000 00:00 0 [vdso]").unwrap();
        assert!(vdso.should_dump);
        let heap = parse_mapping("000055a123400000-000055a123421000 rw-p 00000000 00:00 0 [heap]").unwrap();
        assert!(heap.should_dump);
    }
}
