use super::{CorexError, Mapping, ProcessInfo};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::FileExt;

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

pub(super) fn apply_coredump_filter(
    process: &mut ProcessInfo,
    memory: &File,
) -> Result<(), CorexError> {
    let executable = process.exe.clone();
    for mapping in &mut process.mappings {
        let category = match (mapping.is_file_backed, mapping.is_shared) {
            (false, false) => 0,
            (false, true) => 1,
            (true, false) => 2,
            (true, true) => 3,
        };
        mapping.should_dump = process.coredump_filter & (1 << category) != 0;
        if mapping.should_dump
            || mapping.flags & PF_W != 0
            || (!executable.is_empty() && mapping.path == executable)
        {
            mapping.should_dump = true;
            continue;
        }
        if process.coredump_filter & (1 << 4) != 0
            && mapping.is_file_backed
            && mapping.offset == 0
            && has_elf_magic(memory, mapping.start)?
        {
            mapping.should_dump = true;
        }
    }
    Ok(())
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
    Ok(mappings)
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
    Some(Mapping {
        start,
        end,
        flags,
        offset,
        is_shared: permissions.get(3) == Some(&b's'),
        is_file_backed: inode > 0,
        should_dump: true,
        path,
    })
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

fn has_elf_magic(memory: &File, address: u64) -> Result<bool, CorexError> {
    let mut magic = [0_u8; 4];
    let n = memory
        .read_at(&mut magic, address)
        .map_err(|e| CorexError::io("read ELF magic", format!("{address:#x}"), e))?;
    if n != 4 {
        return Err(CorexError::InvalidData("short ELF magic read".into()));
    }
    Ok(magic == *b"\x7fELF")
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
}
