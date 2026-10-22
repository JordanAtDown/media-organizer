use crate::error::ExifError;
use chrono::{DateTime, Local};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Seconds between the Mac epoch (1904-01-01 UTC) and the Unix epoch.
const MAC_TO_UNIX_EPOCH_SECS: u64 = 2_082_844_800;

/// Read the capture date from a QuickTime/MP4 file's `mvhd` `creation_time`.
///
/// Walks only `moov → mvhd` and never parses `trak`, `mdia`, `stbl` or any other
/// sub-tree. That is deliberate: it stays robust against Apple-specific boxes
/// (`tapt`, `clef`, `prof`, `enof`) that make full-tree parsers fail silently.
///
/// The value is UTC seconds since the Mac epoch, converted to local time.
/// Returns `ExifError::NoDate` if `mvhd` is absent, unreadable, or zero.
pub fn read_quicktime_date(path: &Path) -> Result<DateTime<Local>, ExifError> {
    let file = File::open(path)?;
    read_mvhd_creation_time(&mut BufReader::new(file))
}

fn no_date<E>(_: E) -> ExifError {
    ExifError::NoDate
}

fn skip<R: Seek>(r: &mut R, len: u64) -> Result<(), ExifError> {
    let len = i64::try_from(len).map_err(no_date)?;
    r.seek(SeekFrom::Current(len)).map_err(no_date)?;
    Ok(())
}

fn read_mvhd_creation_time<R: Read + Seek>(r: &mut R) -> Result<DateTime<Local>, ExifError> {
    loop {
        let (box_type, payload_len) = read_box_header(r)?;
        if &box_type == b"moov" {
            return read_mvhd_in_moov(r, payload_len);
        }
        // Skip every other top-level box (ftyp, mdat, free, wide, …).
        skip(r, payload_len)?;
    }
}

fn read_mvhd_in_moov<R: Read + Seek>(
    r: &mut R,
    moov_payload: u64,
) -> Result<DateTime<Local>, ExifError> {
    let start = r.stream_position().map_err(no_date)?;
    let end = start.saturating_add(moov_payload);

    while r.stream_position().map_err(no_date)? + 8 <= end {
        let (box_type, payload_len) = read_box_header(r)?;
        if &box_type == b"mvhd" {
            return parse_mvhd_creation_time(r);
        }
        // Skip trak, udta, meta, and every other moov child.
        skip(r, payload_len)?;
    }
    Err(ExifError::NoDate)
}

fn parse_mvhd_creation_time<R: Read>(r: &mut R) -> Result<DateTime<Local>, ExifError> {
    let mut ver_flags = [0u8; 4];
    r.read_exact(&mut ver_flags).map_err(no_date)?;

    let mac_secs: u64 = match ver_flags[0] {
        0 => {
            let mut buf = [0u8; 4];
            r.read_exact(&mut buf).map_err(no_date)?;
            u32::from_be_bytes(buf) as u64
        }
        1 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf).map_err(no_date)?;
            u64::from_be_bytes(buf)
        }
        _ => return Err(ExifError::NoDate),
    };

    // creation_time == 0 means the encoder never set it. It also guards against
    // the synthetic thumbnails (SYNOPHOTO_FILM_M.mp4) a DSM volume generates.
    if mac_secs == 0 {
        return Err(ExifError::NoDate);
    }
    let unix_secs = mac_secs.saturating_sub(MAC_TO_UNIX_EPOCH_SECS);
    DateTime::from_timestamp(i64::try_from(unix_secs).map_err(no_date)?, 0)
        .map(|dt| dt.with_timezone(&Local))
        .ok_or(ExifError::NoDate)
}

/// Read an ISOBMFF box header.
/// Returns `(box_type, payload_size)` where the payload excludes the header.
fn read_box_header<R: Read>(r: &mut R) -> Result<([u8; 4], u64), ExifError> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header).map_err(no_date)?;
    let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let box_type = [header[4], header[5], header[6], header[7]];
    let payload = match size {
        // Box extends to EOF: nothing after it can be a `moov`, so give up.
        0 => return Err(ExifError::NoDate),
        // 64-bit extended size, stored right after the type.
        1 => {
            let mut ext = [0u8; 8];
            r.read_exact(&mut ext).map_err(no_date)?;
            u64::from_be_bytes(ext).saturating_sub(16)
        }
        s => u64::from(s).saturating_sub(8),
    };
    Ok((box_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // 2026-01-02 11:41:57 UTC.
    const UNIX: u64 = 1_767_354_117;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn ftyp() -> Vec<u8> {
        boxed(b"ftyp", b"isom\x00\x00\x02\x00isom")
    }

    fn mvhd_v0(mac_secs: u32) -> Vec<u8> {
        let mut p = vec![0u8; 4]; // version 0 + flags
        p.extend_from_slice(&mac_secs.to_be_bytes()); // creation_time
        p.extend_from_slice(&mac_secs.to_be_bytes()); // modification_time
        p.extend_from_slice(&[0u8; 92]);
        boxed(b"mvhd", &p)
    }

    fn mvhd_v1(mac_secs: u64) -> Vec<u8> {
        let mut p = vec![1u8, 0, 0, 0];
        p.extend_from_slice(&mac_secs.to_be_bytes());
        p.extend_from_slice(&[0u8; 100]);
        boxed(b"mvhd", &p)
    }

    fn read(data: Vec<u8>) -> Result<DateTime<Local>, ExifError> {
        read_mvhd_creation_time(&mut Cursor::new(data))
    }

    fn mac(unix: u64) -> u32 {
        (unix + MAC_TO_UNIX_EPOCH_SECS) as u32
    }

    #[test]
    fn reads_version_0_creation_time() {
        let mut data = ftyp();
        data.extend(boxed(b"moov", &mvhd_v0(mac(UNIX))));
        assert_eq!(read(data).unwrap().timestamp() as u64, UNIX);
    }

    #[test]
    fn reads_version_1_creation_time() {
        let mut data = ftyp();
        data.extend(boxed(b"moov", &mvhd_v1(UNIX + MAC_TO_UNIX_EPOCH_SECS)));
        assert_eq!(read(data).unwrap().timestamp() as u64, UNIX);
    }

    #[test]
    fn missing_moov_is_no_date() {
        assert!(matches!(read(ftyp()), Err(ExifError::NoDate)));
    }

    #[test]
    fn zero_creation_time_is_rejected() {
        let mut data = ftyp();
        data.extend(boxed(b"moov", &mvhd_v0(0)));
        assert!(matches!(read(data), Err(ExifError::NoDate)));
    }

    #[test]
    fn skips_unrelated_boxes_before_mvhd() {
        let mut moov_payload = boxed(b"tapt", &[0u8; 24]);
        moov_payload.extend(boxed(b"prof", &[0u8; 8]));
        moov_payload.extend(mvhd_v0(mac(UNIX)));
        let mut data = ftyp();
        data.extend(boxed(b"moov", &moov_payload));
        assert_eq!(read(data).unwrap().timestamp() as u64, UNIX);
    }

    #[test]
    fn finds_moov_after_a_large_mdat() {
        let mut data = ftyp();
        data.extend(boxed(b"mdat", &vec![0xAB; 1 << 20]));
        data.extend(boxed(b"moov", &mvhd_v0(mac(UNIX))));
        assert_eq!(read(data).unwrap().timestamp() as u64, UNIX);
    }

    #[test]
    fn handles_64_bit_extended_box_size() {
        // A `free` box using the size == 1 form, followed by the real moov.
        let body = [0u8; 10];
        let mut data = ftyp();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(b"free");
        data.extend_from_slice(&((body.len() + 16) as u64).to_be_bytes());
        data.extend_from_slice(&body);
        data.extend(boxed(b"moov", &mvhd_v0(mac(UNIX))));
        assert_eq!(read(data).unwrap().timestamp() as u64, UNIX);
    }

    #[test]
    fn size_zero_box_gives_up() {
        let mut data = ftyp();
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"mdat");
        assert!(matches!(read(data), Err(ExifError::NoDate)));
    }

    #[test]
    fn truncated_file_is_no_date() {
        assert!(matches!(read(vec![0, 0, 0]), Err(ExifError::NoDate)));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let err = read_quicktime_date(Path::new("/nonexistent/clip.mov")).unwrap_err();
        assert!(matches!(err, ExifError::Io(_)));
    }
}
