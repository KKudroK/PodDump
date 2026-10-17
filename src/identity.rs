use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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

fn parse_container_segment(segment: &str) -> Option<&str> {
    if segment.starts_with("crio-conmon-") {
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
        if let Some(id) = core.strip_prefix(prefix) {
            if is_hex64(id) {
                return Some(id);
            }
        }
    }
    if let Some((_, tail)) = core.rsplit_once('-') {
        if is_hex64(tail) {
            return Some(tail);
        }
    }
    None
}

fn extract_container_id(cgroup: &str, uid: &str) -> Result<String, String> {
    let mut after_pod = false;
    let mut found: Option<&str> = None;
    for seg in cgroup.split('/').filter(|s| !s.is_empty()) {
        if is_pod_segment(seg, uid) {
            after_pod = true;
            continue;
        }
        if after_pod {
            if let Some(id) = parse_container_segment(seg) {
                found = Some(id);
            }
        }
    }
    if let Some(id) = found {
        return Ok(id.to_string());
    }
    // Fallback for custom hierarchies where the 64-hex container scope appears anywhere in path
    if let Some(id) = cgroup
        .split('/')
        .filter_map(parse_container_segment)
        .next_back()
    {
        return Ok(id.to_string());
    }
    Err(format!("ambiguous container attribution: {cgroup}"))
}

fn cgroup_search_roots() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(custom) = std::env::var("PODDUMP_CGROUP_ROOT") {
        if !custom.is_empty() {
            bases.push(PathBuf::from(custom));
        }
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
    if let Some(unified) = cg_contents.lines().find_map(|l| l.strip_prefix("0::")) {
        if member(unified, uid) {
            return Ok(unified.to_string());
        }
    }
    for line in cg_contents.lines() {
        if let Some((_, rest)) = line.split_once(':') {
            if let Some((_, path)) = rest.split_once(':') {
                if member(path, uid) {
                    return Ok(path.to_string());
                }
            }
        }
    }
    if let Some(unified) = cg_contents.lines().find_map(|l| l.strip_prefix("0::")) {
        return Ok(unified.to_string());
    }
    Err("no cgroup entry found".into())
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
        .get(0)
        .and_then(|s| s.chars().next())
        .ok_or("empty state")?;
    let starttime = fields
        .get(19)
        .ok_or("short stat")?
        .parse()
        .map_err(|_| "bad starttime")?;
    Ok((state, starttime))
}

impl Identity {
    pub fn read(pid: i32, uid: &str) -> Result<Self, String> {
        let stat_raw =
            fs::read(format!("/proc/{pid}/stat")).map_err(|e| format!("PID {pid} stat: {e}"))?;
        let (state, starttime) = parse_stat_fields(&stat_raw)?;
        let cg_raw = fs::read(format!("/proc/{pid}/cgroup")).map_err(|e| e.to_string())?;
        let cg = String::from_utf8_lossy(&cg_raw);
        let cgroup = select_proc_cgroup(&cg, uid)?;
        if !member(&cgroup, uid) {
            return Err(format!("PID {pid} no longer belongs to Pod {uid}"));
        }
        let container_id = extract_container_id(&cgroup, uid)?;
        Ok(Self {
            pid,
            starttime,
            cgroup,
            container_id,
            state,
        })
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
    let mut todo = vec![root.to_path_buf()];
    let mut ids = BTreeMap::new();
    let mut errors = Vec::new();
    while let Some(p) = todo.pop() {
        let procs = match fs::read(p.join("cgroup.procs")) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                errors.push(serde_json::json!({"path":p,"error":e.to_string(),"kind":"cgroup_read_failure"}));
                continue;
            }
        };
        for line in procs.lines() {
            let pid = line.parse().map_err(|_| "bad cgroup PID")?;
            match Identity::read(pid, uid) {
                Ok(id) => {
                    ids.insert(pid, id);
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
}
