//! FIFO among registered requests. Caller holds the credit file's flock.
//! OFD slot locks define liveness; stale records after exit are ignored/reaped.
use std::{
    fs::File,
    io,
    os::{fd::AsRawFd, unix::fs::FileExt},
};

pub const SLOTS: usize = 256;
const META: u64 = 8192;
const ROWS: u64 = META + 64;
const LOCKS: i64 = 1 << 47;

#[derive(Clone, Copy, Debug)]
pub struct Ticket {
    pub id: u64,
    slot: usize,
}
#[derive(Default)]
struct State {
    next: u64,
    active: [u8; 32],
}
fn read(file: &File, at: u64, out: &mut [u8]) -> Result<(), String> {
    out.fill(0);
    let mut n = 0;
    while n < out.len() {
        match file.read_at(&mut out[n..], at + n as u64) {
            Ok(0) => break,
            Ok(v) => n += v,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read admission queue: {e}")),
        }
    }
    Ok(())
}
fn state(file: &File) -> Result<State, String> {
    let mut data = [0u8; 40];
    read(file, META, &mut data)?;
    Ok(State {
        next: u64::from_le_bytes(data[..8].try_into().unwrap()),
        active: data[8..].try_into().unwrap(),
    })
}
fn store(file: &File, s: &State) -> Result<(), String> {
    let mut data = [0u8; 40];
    data[..8].copy_from_slice(&s.next.to_le_bytes());
    data[8..].copy_from_slice(&s.active);
    file.write_all_at(&data, META)
        .map_err(|e| format!("write admission queue: {e}"))
}
fn bit(s: &State, i: usize) -> bool {
    s.active[i / 8] & (1 << (i % 8)) != 0
}
fn set(s: &mut State, i: usize, value: bool) {
    if value {
        s.active[i / 8] |= 1 << (i % 8)
    } else {
        s.active[i / 8] &= !(1 << (i % 8))
    }
}
fn row(file: &File, i: usize) -> Result<u64, String> {
    let mut b = [0u8; 8];
    read(file, ROWS + i as u64 * 8, &mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn locked(file: &File, i: usize) -> Result<bool, String> {
    let mut f: libc::flock = unsafe { std::mem::zeroed() };
    f.l_type = libc::F_WRLCK as i16;
    f.l_whence = libc::SEEK_SET as i16;
    f.l_start = LOCKS + i as i64;
    f.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut f) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(f.l_type != libc::F_UNLCK as i16)
}
fn slot_lock(file: &File, i: usize, kind: i16) -> Result<(), String> {
    let mut f: libc::flock = unsafe { std::mem::zeroed() };
    f.l_type = kind;
    f.l_whence = libc::SEEK_SET as i16;
    f.l_start = LOCKS + i as i64;
    f.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &f) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}
pub fn register(file: &File) -> Result<Option<Ticket>, String> {
    let mut s = state(file)?;
    for i in 0..SLOTS {
        if bit(&s, i) && !locked(file, i)? {
            set(&mut s, i, false)
        }
    }
    let Some(slot) = (0..SLOTS).find(|i| !bit(&s, *i)) else {
        return Ok(None);
    };
    let id = s
        .next
        .checked_add(1)
        .ok_or("admission ticket counter exhausted")?;
    slot_lock(file, slot, libc::F_WRLCK as i16)?;
    file.write_all_at(&id.to_le_bytes(), ROWS + slot as u64 * 8)
        .map_err(|e| format!("write admission ticket: {e}"))?;
    s.next = id;
    set(&mut s, slot, true);
    store(file, &s)?;
    Ok(Some(Ticket { id, slot }))
}
/// Return the number of live older registrations. Own OFD never conflicts with
/// its own slot, so it must be recognized explicitly rather than reaped.
pub fn ahead(file: &File, t: Ticket) -> Result<usize, String> {
    let mut s = state(file)?;
    if !bit(&s, t.slot) || row(file, t.slot)? != t.id {
        return Err("admission ticket was changed while queued".into());
    }
    let mut older = 0;
    let mut changed = false;
    for i in 0..SLOTS {
        if i == t.slot || !bit(&s, i) {
            continue;
        }
        if !locked(file, i)? {
            set(&mut s, i, false);
            changed = true;
            continue;
        }
        let id = row(file, i)?;
        if id == 0 || id > s.next || id == t.id {
            return Err("invalid live admission ticket".into());
        }
        if id < t.id {
            older += 1
        }
    }
    if changed {
        store(file, &s)?
    }
    Ok(older)
}
pub fn depart(file: &File, t: Ticket) -> Result<(), String> {
    let mut s = state(file)?;
    if !bit(&s, t.slot) || row(file, t.slot)? != t.id {
        return Err("admission ticket changed before reservation".into());
    }
    set(&mut s, t.slot, false);
    store(file, &s)?;
    slot_lock(file, t.slot, libc::F_UNLCK as i16)
}
#[cfg(test)]
pub fn live(file: &File) -> Result<usize, String> {
    let s = state(file)?;
    let mut count = 0;
    for i in 0..SLOTS {
        if bit(&s, i) && locked(file, i)? {
            count += 1
        }
    }
    Ok(count)
}
