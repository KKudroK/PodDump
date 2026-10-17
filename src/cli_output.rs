//! Render persisted collection results without adding target inspection work.
use serde_json::Value;
use std::{collections::BTreeMap, fmt::Write, path::Path};

struct Process<'a> {
    identity: &'a Value,
    row: Option<&'a Value>,
}

fn text(s: &str) -> String {
    // Keep errors and paths on one terminal line; strip terminal controls.
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn field(v: &Value, key: &str) -> String {
    text(v[key].as_str().unwrap_or("unknown"))
}

pub fn tree(manifest: &Value, output: &Path, elapsed_s: f64, full_ids: bool) -> String {
    let mut groups: BTreeMap<&str, Vec<Process<'_>>> = BTreeMap::new();
    if let Some(rows) = manifest["processes"].as_array() {
        for row in rows {
            let id = &row["identity"];
            groups
                .entry(id["container_id"].as_str().unwrap_or("unresolved"))
                .or_default()
                .push(Process {
                    identity: id,
                    row: Some(row),
                });
        }
    }
    if let Some(ids) = manifest["uncaptured_new"].as_array() {
        for id in ids {
            groups
                .entry(id["container_id"].as_str().unwrap_or("unresolved"))
                .or_default()
                .push(Process {
                    identity: id,
                    row: None,
                });
        }
    }
    let complete = manifest["processes"].as_array().map_or(0, |rows| {
        rows.iter()
            .filter(|row| row["status"] == "complete")
            .count()
    });
    let partial = manifest["processes"].as_array().map_or(0, |rows| {
        rows.iter().filter(|row| row["status"] == "partial").count()
    });
    let captured = complete + partial;
    let total: usize = groups.values().map(Vec::len).sum();
    // Extend colliding prefixes, so abbreviated IDs still distinguish groups.
    let mut prefix = 12;
    while !full_ids
        && groups.keys().enumerate().any(|(i, id)| {
            groups
                .keys()
                .skip(i + 1)
                .any(|other| id.chars().take(prefix).eq(other.chars().take(prefix)))
        })
    {
        prefix += 1;
    }
    let scope = manifest["scope"]
        .as_str()
        .or_else(|| manifest["collection_policy"].as_str())
        .map(text)
        .unwrap_or_else(|| "unknown".into());
    let mut s = format!(
        "PodDump {}\nNode: {}  |  Scope: {}  |  Durability: {}\n\nPod: {}\n",
        field(manifest, "version"),
        field(manifest, "node"),
        scope,
        field(manifest, "durability"),
        field(manifest, "pod_uid")
    );
    let abbreviated = !full_ids && groups.keys().any(|id| id.chars().count() > prefix);
    let group_count = groups.len();
    for (ci, (id, mut processes)) in groups.into_iter().enumerate() {
        let last = ci + 1 == group_count;
        let branch = if last { "└──" } else { "├──" };
        let stem = if last { "    " } else { "│   " };
        let display = if full_ids || id.chars().count() <= prefix {
            text(id)
        } else {
            format!("{}…", text(&id.chars().take(prefix).collect::<String>()))
        };
        writeln!(s, "{branch} Container: {display}").unwrap();
        processes.sort_by_key(|p| (p.identity["pid"].as_i64(), p.identity["starttime"].as_u64()));
        let count = processes.len();
        for (pi, process) in processes.into_iter().enumerate() {
            let branch = if pi + 1 == count {
                "└──"
            } else {
                "├──"
            };
            let pid = process.identity["pid"]
                .as_i64()
                .map_or("unknown".into(), |p| p.to_string());
            match process.row {
                Some(row) if row["status"] == "complete" => {
                    writeln!(
                        s,
                        "{stem}{branch} PID {pid:<7} [OK]      {}",
                        field(row, "file")
                    )
                    .unwrap();
                }
                Some(row) if row["status"] == "partial" => {
                    let file_name = row["file"]
                        .as_str()
                        .or_else(|| row["partial_file"].as_str())
                        .map(text)
                        .unwrap_or_else(|| "unknown".into());
                    writeln!(
                        s,
                        "{stem}{branch} PID {pid:<7} [PARTIAL] {file_name} (durable ELF; see coverage ledger)"
                    )
                    .unwrap();
                }
                Some(row) => {
                    writeln!(
                        s,
                        "{stem}{branch} PID {pid:<7} [FAILED]  {}",
                        field(row, "error")
                    )
                    .unwrap();
                }
                None => {
                    writeln!(
                        s,
                        "{stem}{branch} PID {pid:<7} [MISSED]  Uncaptured identity (start={})",
                        process.identity["starttime"]
                    )
                    .unwrap();
                }
            }
        }
    }
    let census_errors: Vec<_> = manifest["censuses"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| {
            c.get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(c.get("error").into_iter())
        })
        .collect();
    let status = field(manifest, "status").to_ascii_uppercase();
    write!(s, "\nResult: {status}  |  Processes: {captured}/{total}  |  Containers: {group_count}  |  Elapsed: {elapsed_s:.3} s\nOutput: {}\nManifest: {}",
        text(&output.display().to_string()), text(&output.join("manifest.json").display().to_string())).unwrap();
    if abbreviated {
        write!(
            s,
            "\nContainer IDs abbreviated; full IDs are in the manifest (--full-ids to display)."
        )
        .unwrap();
    }
    write!(s, "\nPIDs are host PIDs; sandbox processes are included.").unwrap();
    if manifest["cancel_requested"] == true {
        write!(s, "\nWarning: Collection was cancelled.").unwrap();
    }
    for error in census_errors {
        let msg = error
            .as_str()
            .map(text)
            .unwrap_or_else(|| field(error, "error"));
        write!(s, "\nCensus warning: {msg}").unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tree_displays_collection_policy_and_partial_durable_cores() {
        let manifest = json!({
            "version": "0.1.0",
            "node": "kkudrok-node",
            "collection_policy": "all_mapped_proc_mem",
            "durability": "batch",
            "pod_uid": "pod-1234",
            "status": "partial",
            "processes": [
                {
                    "identity": {"pid": 101, "starttime": 1000, "container_id": "container_alpha_123456"},
                    "status": "complete",
                    "file": "101_1000.core"
                },
                {
                    "identity": {"pid": 102, "starttime": 1001, "container_id": "container_alpha_123456"},
                    "status": "partial",
                    "partial_file": "102_1001.core.partial",
                    "error": "mapped bytes are missing; consult coverage ledger"
                }
            ]
        });
        let rendered = tree(&manifest, Path::new("/var/tmp/out"), 0.42, false);
        assert!(rendered.contains("Scope: all_mapped_proc_mem"));
        assert!(rendered.contains("PID 101     [OK]      101_1000.core"));
        assert!(rendered.contains("PID 102     [PARTIAL] 102_1001.core.partial"));
        assert!(rendered.contains("Processes: 2/2"));
    }
}
