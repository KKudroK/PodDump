use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub pid: i32,
    pub starttime: u64,
    pub cgroup: String,
    pub container_id: String,
    pub state: char,
}
pub fn valid_uid(uid: &str) -> Result<(), String> {
    if uid.len() != 36
        || !uid.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
    {
        return Err("Pod UID must be a full UUID".into());
    }
    Ok(())
}
fn is_pod_segment(segment: &str, uid: &str) -> bool {
    let normalized = uid.replace('-', "_");
    segment == format!("pod{uid}")
        || segment == format!("pod{normalized}")
        || segment.ends_with(&format!("-pod{normalized}.slice"))
        || segment.ends_with(&format!("-pod{uid}.slice"))
}

fn member(cgroup: &str, uid: &str) -> bool {
    cgroup.split('/').any(|s| is_pod_segment(s, uid))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_crio_conmon_helper(cgroup: &str) -> bool {
    cgroup.split('/').any(|segment| {
        segment.starts_with("crio-conmon-")
            || segment.starts_with("libpod-conmon-")
            || segment == "conmon.scope"
    })
}

fn parse_container_segment(segment: &str) -> Option<&str> {
    if segment.starts_with("crio-conmon-") || segment.starts_with("libpod-conmon-") {
        return None;
    }
    let core = segment.strip_suffix(".scope").unwrap_or(segment);
    if is_hex64(core) {
        return Some(core);
    }
    for prefix in [
        "cri-containerd-",
        "crio-",
        "docker-",
        "containerd-",
        "libpod-",
    ] {
        if let Some(id) = core.strip_prefix(prefix)
            && is_hex64(id)
        {
            return Some(id);
        }
    }
    if let Some((_, tail)) = core.rsplit_once('-')
        && is_hex64(tail)
    {
        return Some(tail);
    }
    None
}

fn extract_container_id(cgroup: &str, uid: &str) -> Result<String, String> {
    // Retain custom hierarchy support, but never choose an arbitrary ID from
    // nested container scopes. Duplicate references to the same ID are fine.
    let mut ids: BTreeSet<_> = cgroup
        .split('/')
        .filter_map(parse_container_segment)
        .map(str::to_ascii_lowercase)
        .collect();
    if member(cgroup, uid) && ids.len() == 1 {
        return Ok(ids.pop_first().unwrap());
    }
    Err(format!("ambiguous container attribution: {cgroup}"))
}

fn cgroup_search_roots() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(custom) = std::env::var("PODDUMP_CGROUP_ROOT")
        && !custom.is_empty()
    {
        bases.push(PathBuf::from(custom));
    }
    for candidate in ["/host/sys/fs/cgroup", "/sys/fs/cgroup"] {
        let p = PathBuf::from(candidate);
        if p.is_dir() && !bases.contains(&p) {
            bases.push(p);
        }
    }
    let mut roots = Vec::new();
    for base in bases {
        if base.join("cgroup.controllers").exists() {
            roots.push(base);
        } else {
            for sub in ["pids", "systemd", "memory", "cpu", ""] {
                let r = if sub.is_empty() {
                    base.clone()
                } else {
                    base.join(sub)
                };
                if r.is_dir() && !roots.contains(&r) {
                    roots.push(r);
                }
            }
        }
    }
    roots
}

pub fn resolve(uid: &str) -> Result<PathBuf, String> {
    let roots = cgroup_search_roots();
    if roots.is_empty() {
        return Err(
            "no cgroup filesystem found under /sys/fs/cgroup or /host/sys/fs/cgroup".into(),
        );
    }
    // Exact conventional roots avoid walking every container for every UID.
    // Fallback retains discovery for custom hierarchies; membership is always
    // revalidated from /proc before and after stopping.
    let normalized = uid.replace('-', "_");
    for root in &roots {
        let root_str = root.to_string_lossy();
        let mut standard = Vec::new();
        for qos in ["", "burstable", "besteffort"] {
            let systemd = if qos.is_empty() {
                format!("{root_str}/kubepods.slice/kubepods-pod{normalized}.slice")
            } else {
                format!(
                    "{root_str}/kubepods.slice/kubepods-{qos}.slice/kubepods-{qos}-pod{normalized}.slice"
                )
            };
            let cgroupfs = if qos.is_empty() {
                format!("{root_str}/kubepods/pod{uid}")
            } else {
                format!("{root_str}/kubepods/{qos}/pod{uid}")
            };
            for path in [systemd, cgroupfs] {
                let p = PathBuf::from(path);
                if p.is_dir() {
                    standard.push(p);
                }
            }
        }
        if standard.len() == 1 {
            return Ok(standard.remove(0));
        }
        if standard.len() > 1 {
            return Err("ambiguous standard Pod cgroup roots".into());
        }
    }
    for root in &roots {
        let mut todo = vec![root.clone()];
        let mut hits = Vec::new();
        while let Some(p) = todo.pop() {
            let Ok(entries) = fs::read_dir(&p) else {
                continue;
            };
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let path = e.path();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if is_pod_segment(&name, uid) {
                        hits.push(path);
                    } else {
                        todo.push(path);
                    }
                }
            }
        }
        if hits.len() == 1 {
            return Ok(hits.remove(0));
        }
        if hits.len() > 1 {
            return Err(format!(
                "Pod UID cgroup matches: {}; expected exactly one",
                hits.len()
            ));
        }
    }
    Err("Pod UID cgroup matches: 0; expected exactly one".into())
}

fn select_proc_cgroup(cg_contents: &str, uid: &str) -> Result<String, String> {
    let mut paths = BTreeSet::new();
    for line in cg_contents.lines() {
        let fields: Vec<_> = line.splitn(3, ':').collect();
        if fields.len() != 3 || fields[0].parse::<u32>().is_err() || !fields[2].starts_with('/') {
            return Err("malformed proc cgroup entry".into());
        }
        if member(fields[2], uid) {
            paths.insert(fields[2].to_string());
        }
    }
    if paths.len() > 1 {
        return Err("ambiguous Pod membership across cgroup controllers".into());
    }
    paths
        .pop_first()
        .ok_or_else(|| format!("no cgroup entry belongs to Pod {uid}"))
}

pub(crate) fn parse_stat_fields(stat_raw: &[u8]) -> Result<(char, u64), String> {
    let rparen = stat_raw
        .iter()
        .rposition(|&b| b == b')')
        .ok_or("bad stat: missing closing parenthesis")?;
    let tail_bytes = stat_raw
        .get(rparen + 1..)
        .ok_or("bad stat: missing tail after closing parenthesis")?;
    let tail = std::str::from_utf8(tail_bytes).map_err(|_| "bad stat tail: non-ascii fields")?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|s| s.chars().next())
        .ok_or("empty state")?;
    let starttime = fields
        .get(19)
        .ok_or("short stat")?
        .parse()
        .map_err(|_| "bad starttime")?;
    Ok((state, starttime))
}

pub(crate) enum MemberLookup {
    Member(Identity),
    RuntimeHelper,
}

impl Identity {
    pub fn read(pid: i32, uid: &str) -> Result<Self, String> {
        match Self::read_with(pid, uid, |name| {
            fs::read(format!("/proc/{pid}/{name}")).map_err(|e| format!("PID {pid} {name}: {e}"))
        })? {
            MemberLookup::Member(id) => Ok(id),
            MemberLookup::RuntimeHelper => Err(format!("PID {pid} is a runtime helper (conmon)")),
        }
    }
    pub(crate) fn read_member(pid: i32, uid: &str) -> Result<MemberLookup, String> {
        Self::read_with(pid, uid, |name| {
            fs::read(format!("/proc/{pid}/{name}")).map_err(|e| format!("PID {pid} {name}: {e}"))
        })
    }
    fn read_with(
        pid: i32,
        uid: &str,
        mut read: impl FnMut(&str) -> Result<Vec<u8>, String>,
    ) -> Result<MemberLookup, String> {
        let (_, starttime) = parse_stat_fields(&read("stat")?)?;
        let cg_raw = read("cgroup")?;
        let cg = std::str::from_utf8(&cg_raw).map_err(|_| "non-UTF-8 proc cgroup path")?;
        let cgroup = select_proc_cgroup(cg, uid)?;
        let (state, after) = parse_stat_fields(&read("stat")?)?;
        if after != starttime {
            return Err(format!("PID {pid} replaced during identity read"));
        }
        if !member(&cgroup, uid) {
            return Err(format!("PID {pid} no longer belongs to Pod {uid}"));
        }
        if is_crio_conmon_helper(&cgroup) {
            return Ok(MemberLookup::RuntimeHelper);
        }
        let container_id = extract_container_id(&cgroup, uid)?;
        Ok(MemberLookup::Member(Self {
            pid,
            starttime,
            cgroup,
            container_id,
            state,
        }))
    }
    pub fn verify(&self, uid: &str) -> Result<(), String> {
        let current = Self::read(self.pid, uid)?;
        if current.starttime != self.starttime
            || current.cgroup != self.cgroup
            || current.container_id != self.container_id
        {
            return Err(format!("identity/membership changed for PID {}", self.pid));
        }
        Ok(())
    }
}
pub fn census(root: &Path, uid: &str) -> Result<(Vec<Identity>, Vec<serde_json::Value>), String> {
    census_with(root, uid, Identity::read_member)
}
fn census_with(
    root: &Path,
    uid: &str,
    mut read: impl FnMut(i32, &str) -> Result<MemberLookup, String>,
) -> Result<(Vec<Identity>, Vec<serde_json::Value>), String> {
    let mut todo = vec![root.to_path_buf()];
    let mut ids = BTreeMap::new();
    let mut errors = Vec::new();
    while let Some(p) = todo.pop() {
        let procs = match fs::read(p.join("cgroup.procs")) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                errors.push(serde_json::json!({"path":p,"error":e.to_string(),"kind":"cgroup_read_failure"}));
                // A parent read failure must not hide readable child cgroups.
                String::new()
            }
        };
        for line in procs.lines() {
            let pid = match line.parse::<i32>() {
                Ok(pid) if pid > 0 => pid,
                _ => {
                    errors.push(serde_json::json!({"path":p,"value":line,"error":"invalid cgroup PID","kind":"invalid_cgroup_pid"}));
                    continue;
                }
            };
            match read(pid, uid) {
                Ok(MemberLookup::Member(id)) => {
                    ids.insert(pid, id);
                }
                Ok(MemberLookup::RuntimeHelper) => {
                    // CRI-O/libpod conmon monitor processes live inside the Pod slice
                    // without container payload; skip cleanly instead of polluting errors.
                }
                Err(e) => {
                    errors.push(serde_json::json!({"pid":pid,"error":e,"kind":"exited_or_identity_read_failure"}));
                }
            }
        }
        match fs::read_dir(&p){Ok(entries)=>{for entry in entries{match entry.and_then(|e|e.file_type().map(|t|(e,t))){Ok((e,t))=>{if t.is_dir(){todo.push(e.path())}},Err(e)=>errors.push(serde_json::json!({"path":p,"error":e.to_string(),"kind":"cgroup_entry_read_failure"}))}}},Err(e)=>errors.push(serde_json::json!({"path":p,"error":e.to_string(),"kind":"cgroup_directory_read_failure"}))}
    }
    Ok((ids.into_values().collect(), errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_non_utf8_comm_in_stat() {
        let mut raw_stat = b"12345 (".to_vec();
        raw_stat.extend_from_slice(&[0xff, 0xfe, 0xfd, 0x80]); // invalid UTF-8 bytes in process name
        raw_stat.extend_from_slice(b") S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20");
        let (state, starttime) = parse_stat_fields(&raw_stat).unwrap();
        assert_eq!(state, 'S');
        assert_eq!(starttime, 987654);
    }
    #[test]
    fn rejects_ambiguous_uid() {
        for s in [
            "",
            "abc",
            "../12345678-1234-1234-1234-123456789abc",
            "12345678_1234_1234_1234_123456789abc",
        ] {
            assert!(valid_uid(s).is_err())
        }
    }
    #[test]
    fn cgroup_component_boundaries() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        assert!(member(&format!("/kubepods/pod{uid}/container"), uid));
        assert!(!member(&format!("/kubepods/pod{uid}extra/container"), uid));
        assert!(!member(&format!("/prefixpod{uid}/container"), uid));
    }
    #[test]
    fn supports_multi_runtime_and_cgroup_v1_v2_formats() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        let norm = "12345678_1234_1234_1234_123456789abc";
        let cid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for path in [
            format!(
                "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod{norm}.slice/cri-containerd-{cid}.scope"
            ),
            format!(
                "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod{norm}.slice/crio-{cid}.scope"
            ),
            format!("/kubepods.slice/kubepods-pod{norm}.slice/docker-{cid}.scope"),
            format!("/kubepods/besteffort/pod{uid}/{cid}"),
            format!("/kubepods/pod{uid}/crio-{cid}"),
        ] {
            assert!(member(&path, uid));
            assert_eq!(extract_container_id(&path, uid).unwrap(), cid);
        }
        let conmon_path =
            format!("/kubepods.slice/kubepods-pod{norm}.slice/crio-conmon-{cid}.scope");
        assert!(extract_container_id(&conmon_path, uid).is_err());

        let cg_v1 = format!(
            "11:pids:/kubepods/burstable/pod{uid}/{cid}\n1:name=systemd:/kubepods/burstable/pod{uid}/{cid}\n0::/\n"
        );
        let selected = select_proc_cgroup(&cg_v1, uid).unwrap();
        assert_eq!(extract_container_id(&selected, uid).unwrap(), cid);
    }
    #[test]
    fn conflicting_controller_membership_is_not_order_dependent() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        let a = format!("/kubepods/pod{uid}/{}", "a".repeat(64));
        let b = format!("/kubepods/pod{uid}/{}", "b".repeat(64));
        for cg in [
            format!("7:pids:{a}\n8:memory:{b}\n0::/\n"),
            format!("0::{b}\n7:pids:{a}\n"),
            format!("7:pids:{a}\n0::{b}\n"),
        ] {
            assert!(select_proc_cgroup(&cg, uid).is_err());
        }
        for cg in [
            format!("0::/\n7:pids:{a}\n8:memory:{a}\n"),
            format!("7:pids:{a}\n8:memory:{a}\n0::/\n"),
        ] {
            assert_eq!(select_proc_cgroup(&cg, uid).unwrap(), a);
        }
        assert!(select_proc_cgroup(&format!("bad:pids:{a}"), uid).is_err());
    }
    #[test]
    fn distinct_nested_container_ids_are_not_silently_attributed() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert!(extract_container_id(&format!("/pod{uid}/{a}/{b}"), uid).is_err());
        assert!(extract_container_id(&format!("/{a}/pod{uid}/{b}"), uid).is_err());
        assert_eq!(
            extract_container_id(&format!("/pod{uid}/{a}/{a}"), uid).unwrap(),
            a
        );
    }
    #[test]
    fn identity_read_rejects_pid_reuse_between_stat_and_cgroup() {
        let uid = "12345678-1234-1234-1234-123456789abc";
        for after in [100, 101] {
            let mut reads = 0;
            let id = Identity::read_with(42, uid, |name| {
                if name == "cgroup" {
                    return Ok(format!("0::/pod{uid}/{}", "a".repeat(64)).into_bytes());
                }
                assert_eq!(name, "stat");
                reads += 1;
                let ticks = if reads == 1 { 100 } else { after };
                Ok(format!("42 (name ) test) S {} {ticks}", vec!["0"; 18].join(" ")).into_bytes())
            });
            assert_eq!(reads, 2);
            assert_eq!(id.is_ok(), after == 100);
        }
    }
    #[test]
    fn census_keeps_children_and_other_pids_after_read_errors() {
        let root = std::env::temp_dir().join(format!(
            "poddump_census_{}_{}",
            std::process::id(),
            crate::now()
        ));
        fs::create_dir_all(root.join("child")).unwrap();
        fs::write(root.join("child/cgroup.procs"), "42\nbad\n0\n-1\n43\n").unwrap();
        let uid = "12345678-1234-1234-1234-123456789abc";
        let (ids, errors) = census_with(&root, uid, |pid, _| {
            Ok(MemberLookup::Member(Identity {
                pid,
                starttime: 100,
                cgroup: format!("/pod{uid}/{}", "a".repeat(64)),
                container_id: "a".repeat(64),
                state: 'S',
            }))
        })
        .unwrap();
        assert_eq!(
            ids.iter().map(|id| id.pid).collect::<Vec<_>>(),
            vec![42, 43]
        );
        assert_eq!(errors.len(), 4);
        assert!(errors.iter().any(|e| e["kind"] == "cgroup_read_failure"));
        assert_eq!(
            errors
                .iter()
                .filter(|e| e["kind"] == "invalid_cgroup_pid")
                .count(),
            3
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn live_identity_negative_controls() {
        if let (Ok(uid), Ok(pid)) = (
            std::env::var("PODDUMP_TEST_UID"),
            std::env::var("PODDUMP_TEST_PID"),
        ) {
            let mut id = Identity::read(pid.parse().unwrap(), &uid).unwrap();
            id.verify(&uid).unwrap();
            id.starttime += 1;
            assert!(id.verify(&uid).is_err());
            id.starttime -= 1;
            id.cgroup.push_str("_wrong");
            assert!(id.verify(&uid).is_err());
            assert!(Identity::read(id.pid, "00000000-0000-0000-0000-000000000000").is_err());
        }
    }

    #[test]
    fn crio_conmon_helper_is_filtered_without_census_error() {
        let root = std::env::temp_dir().join(format!(
            "poddump_conmon_census_{}_{}",
            std::process::id(),
            crate::now()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("cgroup.procs"), "101\n102\n").unwrap();
        let uid = "12345678-1234-1234-1234-123456789abc";
        let norm = "12345678_1234_1234_1234_123456789abc";
        let cid = "a".repeat(64);
        let (ids, errors) = census_with(&root, uid, |pid, u| {
            Identity::read_with(pid, u, |name| match name {
                "stat" => Ok(format!("{pid} (app) S {} 55", vec!["0"; 18].join(" ")).into_bytes()),
                "cgroup" => Ok(if pid == 101 {
                    format!("0::/kubepods.slice/kubepods-pod{norm}.slice/crio-conmon-{cid}.scope\n")
                } else {
                    format!("0::/kubepods.slice/kubepods-pod{norm}.slice/crio-{cid}.scope\n")
                }
                .into_bytes()),
                _ => unreachable!(),
            })
        })
        .unwrap();
        assert!(errors.is_empty(), "conmon helper must not record error");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].pid, 102);
        fs::remove_dir_all(root).unwrap();
    }
}
