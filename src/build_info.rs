use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
};

pub fn hash_reader(mut file: impl Read) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn hash_sparse_file(mut file: File) -> io::Result<String> {
    use std::io::{Seek, SeekFrom};
    use std::os::fd::AsRawFd;

    static ZERO_SLICE: [u8; 65536] = [0u8; 65536];
    let total_len = file.seek(SeekFrom::End(0))?;
    if total_len == 0 {
        return Ok(format!("{:x}", Sha256::new().finalize()));
    }
    if libc::off_t::try_from(total_len).is_err() {
        file.seek(SeekFrom::Start(0))?;
        return hash_reader(file);
    }
    let fd = file.as_raw_fd();
    let probe = unsafe { libc::lseek(fd, 0, libc::SEEK_DATA) };
    if probe < 0 {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENXIO) => {
                let mut hash = Sha256::new();
                let mut rem = total_len;
                while rem > 0 {
                    let step = rem.min(ZERO_SLICE.len() as u64) as usize;
                    hash.update(&ZERO_SLICE[..step]);
                    rem -= step as u64;
                }
                return Ok(format!("{:x}", hash.finalize()));
            }
            Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => {
                file.seek(SeekFrom::Start(0))?;
                return hash_reader(file);
            }
            _ => return Err(err),
        }
    }

    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 262_144];
    let mut pos = 0u64;
    let feed_zeros = |hash: &mut Sha256, mut rem: u64| {
        while rem > 0 {
            let step = rem.min(ZERO_SLICE.len() as u64) as usize;
            hash.update(&ZERO_SLICE[..step]);
            rem -= step as u64;
        }
    };

    while pos < total_len {
        let data_off = unsafe { libc::lseek(fd, pos as libc::off_t, libc::SEEK_DATA) };
        if data_off < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENXIO) {
                feed_zeros(&mut hash, total_len - pos);
                break;
            }
            return Err(err);
        }
        let data_start = (data_off as u64).min(total_len);
        if data_start > pos {
            feed_zeros(&mut hash, data_start - pos);
        }
        if data_start >= total_len {
            break;
        }
        let hole_off = unsafe { libc::lseek(fd, data_start as libc::off_t, libc::SEEK_HOLE) };
        if hole_off < 0 {
            return Err(io::Error::last_os_error());
        }
        let hole_start = (hole_off as u64).clamp(data_start, total_len);
        if hole_start == data_start {
            file.seek(SeekFrom::Start(data_start))?;
            let mut rem = total_len - data_start;
            while rem > 0 {
                let to_read = rem.min(buf.len() as u64) as usize;
                match file.read(&mut buf[..to_read]) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "sparse digest hit unexpected EOF",
                        ));
                    }
                    Ok(n) => {
                        hash.update(&buf[..n]);
                        rem -= n as u64;
                    }
                }
            }
            break;
        }
        file.seek(SeekFrom::Start(data_start))?;
        let mut cur = data_start;
        while cur < hole_start {
            let to_read = (hole_start - cur).min(buf.len() as u64) as usize;
            match file.read(&mut buf[..to_read]) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "sparse digest hit unexpected EOF inside data extent",
                    ));
                }
                Ok(n) => {
                    hash.update(&buf[..n]);
                    cur += n as u64;
                }
            }
        }
        pos = hole_start;
    }

    Ok(format!("{:x}", hash.finalize()))
}
fn executable_sha256() -> io::Result<String> {
    hash_reader(File::open("/proc/self/exe")?)
}
pub fn current() -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/build_info.json")))
            .expect("generated build metadata");
    match executable_sha256() {
        Ok(hash) => value["runtime_executable_sha256"] = serde_json::json!(hash),
        Err(e) => value["runtime_executable_hash_error"] = serde_json::json!(e.to_string()),
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn hash_sparse_file_matches_hash_reader_across_hole_layouts() {
        for (name, setup) in [
            ("empty", 0u64),
            ("all_hole", 512 * 1024),
            ("mixed_holes", 1024 * 1024),
            ("dense", 600 * 1024),
        ] {
            let path = std::env::temp_dir().join(format!(
                "poddump_sparse_hash_{name}_{}_{}",
                std::process::id(),
                crate::now()
            ));
            {
                let mut f = File::create(&path).unwrap();
                match name {
                    "empty" => {}
                    "all_hole" => f.set_len(setup).unwrap(),
                    "mixed_holes" => {
                        // Leading 128 KiB hole, small 4 KiB data, 256 KiB interior hole,
                        // 300 KiB dense data (> 256 KiB read chunk), trailing hole to 1 MiB.
                        f.seek(SeekFrom::Start(128 * 1024)).unwrap();
                        f.write_all(&[0x3c; 4096]).unwrap();
                        f.seek(SeekFrom::Start(388 * 1024)).unwrap();
                        let dense: Vec<u8> = (0..(300 * 1024)).map(|i| (i % 251) as u8).collect();
                        f.write_all(&dense).unwrap();
                        f.set_len(setup).unwrap();
                    }
                    "dense" => {
                        let dense: Vec<u8> = (0..setup as usize).map(|i| (i % 241) as u8).collect();
                        f.write_all(&dense).unwrap();
                    }
                    _ => unreachable!(),
                }
                f.sync_all().unwrap();
            }
            let expected = hash_reader(File::open(&path).unwrap()).unwrap();
            let actual = hash_sparse_file(File::open(&path).unwrap()).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert_eq!(actual, expected, "mismatch for {name}");
        }
    }
}
