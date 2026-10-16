//! Byte admission is held across capture, queueing and writing. Backlog credits
//! survive until fsync/publication. No credit wait is permitted while traced.
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const MIB: usize = 1024 * 1024;
#[derive(Default)]
struct Usage {
    memory: usize,
    backlog: usize,
    files: usize,
    peak_memory: usize,
    peak_backlog: usize,
    peak_files: usize,
}
pub struct Budget {
    memory: usize,
    backlog: usize,
    files: usize,
    usage: Mutex<Usage>,
    wake: Condvar,
}
pub struct Request {
    pub id: crate::identity::Identity,
    pub estimate: usize,
    pub error: Option<String>,
    skips: usize,
    enqueued_ns: u128,
    selected_ns: u128,
    wait_ns: u128,
    blocked: [u64; 3],
}
impl Request {
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"enqueued_ns":self.enqueued_ns,"selected_ns":self.selected_ns,"worker_wait_ns":self.wait_ns,"bypass_count":self.skips,"blocked_checks_memory_backlog_files":self.blocked,"estimate_bytes":self.estimate})
    }

    pub fn new(id: crate::identity::Identity, estimate: Result<usize, String>) -> Self {
        let (estimate, error) = match estimate {
            Ok(n) => (n, None),
            Err(e) => (0, Some(e)),
        };
        Self {
            id,
            estimate,
            error,
            skips: 0,
            enqueued_ns: crate::now(),
            selected_ns: 0,
            wait_ns: 0,
            blocked: [0; 3],
        }
    }
}
pub struct Lease {
    budget: Arc<Budget>,
    memory: usize,
    backlog: usize,
}
impl Budget {
    pub fn new(memory: usize, backlog: usize, files: usize) -> Arc<Self> {
        Arc::new(Self {
            memory,
            backlog,
            files,
            usage: Mutex::new(Usage::default()),
            wake: Condvar::new(),
        })
    }
    pub fn next(
        self: &Arc<Self>,
        queue: &Mutex<VecDeque<Request>>,
        ceiling: usize,
        stream: bool,
    ) -> Option<(Request, Lease, bool)> {
        let wait_begin = crate::now();
        let mut blocked = [0u64; 3];
        let mut usage = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                return None;
            }
            let cancelled = crate::CANCEL.load(crate::Ordering::Relaxed);
            let choice = q.iter().enumerate().find_map(|(i, r)| {
                // Stop bypassing an old request after 16 admissions: drain the
                // finite outstanding work, then the oldest request can run.
                if i > 0 && q[0].skips >= 16 {
                    return None;
                }
                let buffered = !stream && r.estimate <= ceiling.min(self.memory) && r.estimate > 0;
                let memory = if buffered { r.estimate } else { 0 };
                // Oversized output monopolizes backlog; the explicit exception
                // is recorded, not misrepresented as a strict disk byte cap.
                let backlog = r.estimate.min(self.backlog);
                (cancelled
                    || (usage.memory + memory <= self.memory
                        && usage.backlog + backlog <= self.backlog
                        && usage.files < self.files))
                    .then_some((i, memory, backlog, buffered))
            });
            if let Some((i, memory, backlog, buffered)) = choice {
                for r in q.iter_mut().take(i) {
                    r.skips += 1;
                }
                let mut request = q.remove(i).unwrap();
                request.selected_ns = crate::now();
                request.wait_ns = request.selected_ns - wait_begin;
                request.blocked = blocked;
                let (memory, backlog) = if cancelled { (0, 0) } else { (memory, backlog) };
                usage.memory += memory;
                usage.backlog += backlog;
                usage.files += 1;
                usage.peak_memory = usage.peak_memory.max(usage.memory);
                usage.peak_backlog = usage.peak_backlog.max(usage.backlog);
                usage.peak_files = usage.peak_files.max(usage.files);
                return Some((
                    request,
                    Lease {
                        budget: self.clone(),
                        memory,
                        backlog,
                    },
                    buffered && !cancelled,
                ));
            }
            let r = &q[0];
            let mem = if !stream && r.estimate <= ceiling.min(self.memory) {
                r.estimate
            } else {
                0
            };
            blocked[0] += u64::from(usage.memory + mem > self.memory);
            blocked[1] += u64::from(usage.backlog + r.estimate.min(self.backlog) > self.backlog);
            blocked[2] += u64::from(usage.files >= self.files);
            drop(q);
            usage = self
                .wake
                .wait_timeout(usage, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    pub fn metrics(&self) -> serde_json::Value {
        let s = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"snapshot_peak_bytes":s.peak_memory,"backlog_peak_credits":s.peak_backlog,"pending_peak_files":s.peak_files,"snapshot_remaining_bytes":s.memory,"backlog_remaining_credits":s.backlog,"pending_remaining_files":s.files,"snapshot_budget_bytes":self.memory,"backlog_budget_bytes":self.backlog,"backlog_oversize_policy":"one output larger than budget may hold all backlog credits; actual bytes recorded per process","selection":"oldest-fitting; at most16 bypass admissions before draining"})
    }
    /// Census boundaries include processes born while the preceding batch is
    /// writing. This drains a logical census batch, not fixed-size waves.
    pub fn wait_idle(&self) {
        let mut usage = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        while usage.files != 0 {
            usage = self.wake.wait(usage).unwrap_or_else(|e| e.into_inner());
        }
    }
}
impl Lease {
    pub fn release_memory(&mut self) {
        let mut s = self.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        s.memory -= self.memory;
        self.memory = 0;
        self.budget.wake.notify_all();
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        s.memory -= self.memory;
        s.backlog -= self.backlog;
        s.files -= 1;
        self.budget.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(n: usize) -> Request {
        Request::new(
            crate::identity::Identity {
                pid: 1,
                starttime: 1,
                cgroup: String::new(),
                container_id: String::new(),
                state: 'S',
            },
            Ok(n),
        )
    }
    #[test]
    fn queued_snapshot_owns_credit_until_write_and_backlog_until_sync() {
        let b = Budget::new(512 * MIB, 1024 * MIB, 16);
        let q = Mutex::new(VecDeque::from([request(200 * MIB), request(200 * MIB)]));
        let (_, mut first, buffered) = b.next(&q, 512 * MIB, false).unwrap();
        assert!(buffered);
        let (_, second, _) = b.next(&q, 512 * MIB, false).unwrap();
        assert_eq!(b.metrics()["snapshot_remaining_bytes"], 400 * MIB);
        first.release_memory();
        assert_eq!(b.metrics()["snapshot_remaining_bytes"], 200 * MIB);
        assert_eq!(b.metrics()["backlog_remaining_credits"], 400 * MIB);
        drop(first);
        drop(second);
        assert_eq!(b.metrics()["pending_remaining_files"], 0);
    }
    #[test]
    fn oversized_stream_has_no_snapshot_credit_and_exclusive_backlog() {
        let b = Budget::new(8 * MIB, 16 * MIB, 4);
        let q = Mutex::new(VecDeque::from([request(32 * MIB)]));
        let (_, lease, buffered) = b.next(&q, 8 * MIB, false).unwrap();
        assert!(!buffered);
        assert_eq!(b.metrics()["snapshot_remaining_bytes"], 0);
        assert_eq!(b.metrics()["backlog_remaining_credits"], 16 * MIB);
        drop(lease);
    }
}
