#![allow(unsafe_code)]

use super::procfs;
use super::{CorexError, ThreadState};
use std::collections::HashSet;
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::time::{Duration, Instant};

const WAIT_ALL_THREADS: i32 = 0x4000_0000;
const NT_PRSTATUS: usize = 1;
const NT_PRFPREG: usize = 2;
#[cfg(target_arch = "aarch64")]
const NT_ARM_PAC_MASK: usize = 0x406;

pub(super) struct AttachedThreads {
    tgid: i32,
    tids: Vec<i32>,
    stopped: HashSet<i32>,
    preserve_stop: bool,
    pending: std::collections::HashMap<i32, i32>,
}
impl AttachedThreads {
    pub(super) fn attach_process(
        pid: i32,
        cancellation: Option<&crate::engine::CancellationToken>,
    ) -> Result<Self, CorexError> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|e| CorexError::io("read status", pid.to_string(), e))?;
        let preserve_stop = status
            .lines()
            .any(|s| s.starts_with("State:") && s.split_whitespace().nth(1) == Some("T"));
        let mut guard = Self {
            tgid: pid,
            tids: Vec::new(),
            stopped: HashSet::new(),
            preserve_stop,
            pending: std::collections::HashMap::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let attempt = (|| {
            loop {
                check_cancelled(cancellation)?;
                if Instant::now() > deadline {
                    return Err(CorexError::Ptrace("thread census deadline exceeded".into()));
                }
                let tids = procfs::enumerate_threads(pid)?;
                let additions: Vec<_> = tids
                    .iter()
                    .filter(|t| !guard.tids.contains(t))
                    .copied()
                    .collect();
                if additions.is_empty() {
                    break;
                }
                for tid in additions {
                    let rc = unsafe { libc::ptrace(libc::PTRACE_SEIZE, tid, 0, 0) };
                    if rc < 0 {
                        let e = io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::ESRCH) {
                            continue;
                        }
                        return Err(CorexError::Ptrace(format!("seize TID {tid}: {e}")));
                    }
                    guard.tids.push(tid);
                    if unsafe { libc::ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0) } < 0 {
                        return Err(CorexError::Ptrace(format!(
                            "interrupt TID {tid}: {}",
                            io::Error::last_os_error()
                        )));
                    }
                }
                for tid in guard.tids.clone() {
                    if !guard.stopped.contains(&tid) {
                        let pending = wait_stopped(tid, cancellation, deadline)?;
                        guard.pending.insert(tid, pending);
                        guard.stopped.insert(tid);
                    }
                }
            }
            Ok::<_, CorexError>(())
        })();
        if let Err(error) = attempt {
            return match guard.detach_all() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(CorexError::Ptrace(format!(
                    "{error}; attach cleanup: {cleanup}"
                ))),
            };
        }
        guard.tids.sort_unstable_by_key(|t| (*t != pid, *t));
        Ok(guard)
    }
    pub(super) fn tids(&self) -> &[i32] {
        &self.tids
    }
    pub(super) fn preserved_group_stop(&self) -> bool {
        self.preserve_stop
    }
    pub(super) fn finish(mut self) -> Result<(), CorexError> {
        self.detach_all()
    }
    fn detach_all(&mut self) -> Result<(), CorexError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut unresolved = Vec::new();
        let mut errors = Vec::new();
        while let Some(tid) = self.tids.pop() {
            if !self.stopped.contains(&tid) {
                unsafe {
                    libc::ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0);
                }
                match wait_stopped(tid, None, deadline) {
                    Ok(sig) => {
                        self.pending.insert(tid, sig);
                        self.stopped.insert(tid);
                    }
                    Err(e) => {
                        errors.push(e.to_string());
                    }
                }
            }
            let pending = *self.pending.get(&tid).unwrap_or(&0);
            let sig = if self.preserve_stop {
                libc::SIGSTOP
            } else {
                pending
            };
            let mut released = false;
            loop {
                let rc = unsafe { libc::ptrace(libc::PTRACE_DETACH, tid, 0, sig) };
                if rc >= 0 {
                    released = true;
                    break;
                }
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ESRCH) {
                    let status = std::fs::read_to_string(format!("/proc/{tid}/status"));
                    if status
                        .as_ref()
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
                        || status.as_ref().is_ok_and(|s| {
                            s.lines().any(|l| {
                                l.starts_with("TracerPid:")
                                    && l.split_whitespace().nth(1) == Some("0")
                            })
                        })
                    {
                        released = true;
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    errors.push(format!("detach TID {tid}: {e}"));
                    break;
                }
                std::thread::sleep(Duration::from_micros(100));
            }
            if released && self.preserve_stop && pending != 0 && pending != libc::SIGSTOP {
                let rc = unsafe { libc::syscall(libc::SYS_tgkill, self.tgid, tid, pending) };
                if rc < 0 {
                    errors.push(format!(
                        "restore pending signal TID {tid}: {}",
                        io::Error::last_os_error()
                    ));
                }
            }
            if !released {
                unresolved.push(tid);
            }
        }
        self.tids = unresolved;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CorexError::Ptrace(errors.join("; ")))
        }
    }
}
impl Drop for AttachedThreads {
    fn drop(&mut self) {
        if let Err(e) = self.detach_all() {
            eprintln!("cleanup: {e}")
        }
    }
}

pub(super) fn read_thread_states(
    tids: &[i32],
    cancellation: Option<&crate::engine::CancellationToken>,
) -> Result<Vec<ThreadState>, CorexError> {
    tids.iter()
        .map(|tid| {
            check_cancelled(cancellation)?;
            Ok(ThreadState {
                tid: *tid,
                gp_regs: read_regset(*tid, NT_PRSTATUS, gp_regset_size())?,
                fp_regs: read_regset(*tid, NT_PRFPREG, fp_regset_size())?,
                pac_mask: read_pac_mask(*tid),
                xstate: read_xstate(*tid)?,
            })
        })
        .collect()
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

fn wait_stopped(
    tid: i32,
    cancellation: Option<&crate::engine::CancellationToken>,
    deadline: Instant,
) -> Result<i32, CorexError> {
    let mut status = 0;
    loop {
        check_cancelled(cancellation)?;
        let result = unsafe { libc::waitpid(tid, &mut status, WAIT_ALL_THREADS | libc::WNOHANG) };
        if result == tid {
            break;
        }
        if result == 0 {
            if Instant::now() >= deadline {
                return Err(CorexError::Ptrace(format!(
                    "timed out waiting for thread {tid} to stop"
                )));
            }
            std::thread::sleep(Duration::from_micros(50));
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(CorexError::Ptrace(format!(
                "failed to wait for thread {tid}: {error}"
            )));
        }
    }
    if !libc::WIFSTOPPED(status) {
        return Err(CorexError::Ptrace(format!(
            "thread {tid} did not stop after ptrace attach"
        )));
    }
    let event = status >> 16;
    Ok(if event == 0 {
        libc::WSTOPSIG(status)
    } else {
        0
    })
}

fn read_regset(tid: i32, note: usize, expected_size: usize) -> Result<Vec<u8>, CorexError> {
    let mut data = vec![0_u8; expected_size];
    let mut iovec = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            tid,
            note as *mut c_void,
            (&raw mut iovec).cast::<c_void>(),
        )
    };
    if result == -1 {
        return Err(CorexError::Ptrace(format!(
            "failed to read register set {note:#x} for thread {tid}: {}",
            io::Error::last_os_error()
        )));
    }
    if iovec.iov_len != expected_size {
        return Err(CorexError::Ptrace(format!(
            "register set {note:#x} for thread {tid} had size {}, expected {expected_size}",
            iovec.iov_len
        )));
    }
    Ok(data)
}

#[cfg(target_arch = "aarch64")]
fn read_pac_mask(tid: i32) -> Option<[u64; 2]> {
    let data = read_regset(tid, NT_ARM_PAC_MASK, size_of::<[u64; 2]>()).ok()?;
    Some([
        u64::from_ne_bytes(data[0..8].try_into().unwrap()),
        u64::from_ne_bytes(data[8..16].try_into().unwrap()),
    ])
}

#[cfg(not(target_arch = "aarch64"))]
fn read_pac_mask(_tid: i32) -> Option<[u64; 2]> {
    None
}

#[cfg(target_arch = "aarch64")]
const fn gp_regset_size() -> usize {
    size_of::<libc::user_regs_struct>()
}

#[cfg(target_arch = "aarch64")]
const fn fp_regset_size() -> usize {
    size_of::<libc::user_fpsimd_struct>()
}

#[cfg(target_arch = "x86_64")]
const fn gp_regset_size() -> usize {
    size_of::<libc::user_regs_struct>()
}

#[cfg(target_arch = "x86_64")]
const fn fp_regset_size() -> usize {
    size_of::<libc::user_fpregs_struct>()
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("Rust corex supports only aarch64 and x86_64");

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_register_layouts_match_kernel_uapi() {
        assert_eq!(gp_regset_size(), 272);
        assert_eq!(fp_regset_size(), 528);
    }

    #[test]
    fn cancellation_stops_corex_work() {
        let cancellation = crate::engine::CancellationToken::default();
        cancellation.cancel();

        assert!(matches!(
            check_cancelled(Some(&cancellation)),
            Err(CorexError::Cancelled)
        ));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_register_layouts_match_kernel_uapi() {
        assert_eq!(gp_regset_size(), 216);
        assert_eq!(fp_regset_size(), 512);
    }
}

#[cfg(target_arch = "x86_64")]
fn read_xstate(tid: i32) -> Result<Vec<u8>, CorexError> {
    let mut data = vec![0u8; 65536];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let rc = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            tid,
            0x202usize as *mut c_void,
            (&raw mut iov).cast::<c_void>(),
        )
    };
    if rc < 0 {
        return Err(CorexError::Ptrace(format!(
            "XSAVE TID {tid}: {}",
            io::Error::last_os_error()
        )));
    }
    if iov.iov_len < 576 || iov.iov_len >= data.len() {
        return Err(CorexError::InvalidData(format!(
            "XSAVE TID {tid} has unsupported or potentially truncated size {}",
            iov.iov_len
        )));
    }
    data.truncate(iov.iov_len);
    Ok(data)
}
#[cfg(not(target_arch = "x86_64"))]
fn read_xstate(_: i32) -> Result<Vec<u8>, CorexError> {
    Ok(Vec::new())
}
