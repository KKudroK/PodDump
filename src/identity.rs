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
fn member(cgroup: &str, uid: &str) -> bool {
    let normalized = uid.replace('-', "_");
    cgroup
        .split('/')
        .any(|s| s == format!("pod{uid}") || s.ends_with(&format!("-pod{normalized}.slice")))
}
pub fn resolve(uid: &str) -> Result<PathBuf, String> {
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return Err("only cgroup v2 is supported".into());
    }
    let mut todo = vec![PathBuf::from("/sys/fs/cgroup")];
    let mut hits = Vec::new();
    while let Some(p) = todo.pop() {
        for e in fs::read_dir(&p).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            if e.file_type().map_err(|e| e.to_string())?.is_dir() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name == format!("pod{uid}")
                    || name.ends_with(&format!("-pod{}.slice", uid.replace('-', "_")))
                {
                    hits.push(path)
                } else {
                    todo.push(path)
                }
            }
        }
    }
    if hits.len() != 1 {
        return Err(format!(
            "Pod UID cgroup matches: {}; expected exactly one",
            hits.len()
        ));
    }
    Ok(hits.remove(0))
}
impl Identity {
    pub fn read(pid: i32, uid: &str) -> Result<Self, String> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|e| format!("PID {pid} stat: {e}"))?;
        let fields: Vec<_> = stat
            .get(stat.rfind(')').ok_or("bad stat")? + 1..)
            .ok_or("bad stat")?
            .split_whitespace()
            .collect();
        let starttime = fields
            .get(19)
            .ok_or("short stat")?
            .parse()
            .map_err(|_| "bad starttime")?;
        let cg = fs::read_to_string(format!("/proc/{pid}/cgroup")).map_err(|e| e.to_string())?;
        let cgroup = cg
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .ok_or("no unified cgroup")?
            .to_string();
        if !member(&cgroup, uid) {
            return Err(format!("PID {pid} no longer belongs to Pod {uid}"));
        }
        let container_id = cgroup
            .split('/')
            .filter_map(|s| {
                s.strip_prefix("cri-containerd-")
                    .and_then(|s| s.strip_suffix(".scope"))
                    .or_else(|| {
                        (s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
                    })
            })
            .last()
            .ok_or_else(|| format!("ambiguous container attribution: {cgroup}"))?
            .to_string();
        Ok(Self {
            pid,
            starttime,
            cgroup,
            container_id,
            state: fields[0].chars().next().ok_or("empty state")?,
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
        let procs = match fs::read_to_string(p.join("cgroup.procs")) {
            Ok(s) => s,
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
