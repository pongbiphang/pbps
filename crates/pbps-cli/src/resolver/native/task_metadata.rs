//! Decode identity/credential fields without interpreting opaque task names.

use super::UnqualifiedProcess;
use std::fs::File;
use std::io::{self, Read};

pub(super) fn read_bytes(file: File, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized proc record",
        ));
    }
    Ok(bytes)
}

pub(super) fn stat_fields(stat: &[u8]) -> Result<(u32, &str), UnqualifiedProcess> {
    let split = stat
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(UnqualifiedProcess)?;
    let end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or(UnqualifiedProcess)?;
    if stat.get(split + 1) != Some(&b'(') || end <= split + 1 {
        return Err(UnqualifiedProcess);
    }
    let number = std::str::from_utf8(&stat[..split])
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|number| *number > 0)
        .ok_or(UnqualifiedProcess)?;
    // comm can contain non-UTF-8, whitespace and ')' bytes. Only the final
    // delimiter separates it from the kernel's textual identity fields.
    let fields = std::str::from_utf8(&stat[end + 1..]).map_err(|_| UnqualifiedProcess)?;
    let state = fields.split_whitespace().next().ok_or(UnqualifiedProcess)?;
    if !fields.starts_with(' ') || state.len() != 1 || !state.as_bytes()[0].is_ascii_alphabetic() {
        return Err(UnqualifiedProcess);
    }
    Ok((number, fields))
}

pub(super) fn status_fields(status: &[u8]) -> Result<&str, UnqualifiedProcess> {
    if !status.starts_with(b"Name:\t") {
        return Err(UnqualifiedProcess);
    }
    // The kernel escapes newlines and backslashes in Name, but not arbitrary
    // bytes. Omit that unused first field; every identity/credential byte
    // still requires valid text and its caller's ordinary strict parsing.
    let end = status
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or(UnqualifiedProcess)?;
    std::str::from_utf8(&status[end + 1..]).map_err(|_| UnqualifiedProcess)
}

pub(super) fn read_status(file: File, limit: usize) -> Result<String, UnqualifiedProcess> {
    let bytes = read_bytes(file, limit).map_err(|_| UnqualifiedProcess)?;
    Ok(status_fields(&bytes)?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_limit_includes_the_opaque_name() {
        let path = std::env::temp_dir().join(format!("pbps-status-{}", rand::random::<u64>()));
        let bytes = [b"Name:\t".as_slice(), &[0xff; 32], b"\nPid:\t12\n"].concat();
        std::fs::write(&path, &bytes).unwrap();
        let full = File::open(&path).unwrap();
        let short = File::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(read_status(full, bytes.len()).unwrap(), "Pid:\t12\n");
        assert!(read_status(short, bytes.len() - 1).is_err());
    }

    #[test]
    fn only_the_unused_name_can_contain_opaque_bytes() {
        assert_eq!(
            stat_fields(b"12 (a\xff)\n() R 1 2").unwrap(),
            (12, " R 1 2")
        );
        for bad in [
            b"\xff (name) R 1".as_slice(),
            b"0 (name) R 1",
            b"12 name) R 1",
            b"12 (name)\xff 1",
            b"12 (name) RR 1",
            b"12 (name) 1 2",
            b"12 (name) R \xff",
        ] {
            assert!(stat_fields(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            status_fields(b"Name:\tx\xff)(\\n\t\\\\\nPid:\t12\nUid:\t999 999 999 999\n").unwrap(),
            "Pid:\t12\nUid:\t999 999 999 999\n"
        );
        for bad in [
            b"Name:\tx\xff".as_slice(),
            b"Pid:\t12\n",
            b"Name:\tx\nPid:\t\xff\n",
            b"Name:\tx\nUid:\t999 \xff 999 999\n",
            b"Name:\tx\nCapEff:\t\xff\n",
        ] {
            assert!(status_fields(bad).is_err(), "{bad:?}");
        }
    }
}
