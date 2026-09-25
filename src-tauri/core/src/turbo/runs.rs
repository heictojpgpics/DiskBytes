//! Turbo engine — run-list decoding (NTFS data runs).
//!
//! Ported pattern (not code) from `MFTool`
//! `parser/src/lib.rs:314-436` via
//! `resources/MFTool-main/documentation/01_mft_parsing_reference.md` §8
//! (Apache-2.0 reference; algorithms/format facts, original expression):
//! - split byte: low nibble = length width, high nibble = Δ-offset width
//! - terminator `0x00`
//! - length unsigned LE, offset SIGNED LE (sign-extended)
//! - running LCN accumulation; sparse runs (no offset field) surface as
//!   `lcn: None` instead of `MFTool`'s `-1` sentinel (the adaptation doc's
//!   footgun fix)
//! - bounds: attribute end, malformed widths > 8, sanity vs the VCN span

/// One decoded run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEntry {
    /// Length in clusters.
    pub length: u64,
    /// Starting LCN; `None` for sparse runs.
    pub lcn: Option<i64>,
    /// First VCN covered by this run.
    pub first_vcn: u64,
}

/// Run-list decode errors (typed, never silent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The run list overruns the attribute bounds.
    Overrun,
}

/// Decode the run list in `bytes[start..end]` covering the VCN span
/// `[first_vcn, last_vcn]`.
///
/// # Errors
/// [`RunError`] variants for corrupt run lists.
pub fn decode_run_list(
    bytes: &[u8],
    start: usize,
    end: usize,
    first_vcn: u64,
    last_vcn: u64,
) -> Result<Vec<RunEntry>, RunError> {
    let mut out: Vec<RunEntry> = Vec::new();
    let mut cursor = start;
    let end = end.min(bytes.len());
    let mut remaining: i128 = i128::from(last_vcn) - i128::from(first_vcn) + 1;
    let mut lcn: i64 = 0;
    while cursor < end {
        let header = bytes[cursor];
        cursor += 1;
        if header == 0 {
            break; // Terminator.
        }
        let length_width = header & 0x0F;
        let offset_width = (header >> 4) & 0x0F;
        if length_width > 8 || offset_width > 8 {
            // Corrupt widths: skip the claimed bytes, keep the walk alive.
            let skip = usize::from(length_width) + usize::from(offset_width);
            cursor = cursor.saturating_add(skip);
            if cursor > end {
                return Err(RunError::Overrun);
            }
            continue;
        }
        // Length: unsigned LE.
        let length = read_le_u64(bytes, &mut cursor, length_width).ok_or(RunError::Overrun)?;
        // A real run list never contains a zero-length run (the format
        // has no such encoding) — a 0 here means corrupt bytes, and
        // emitting it downstream (a length-0 "extent") would violate
        // every consumer's length>0 expectation. Skip the run, keep the
        // walk alive (found by the proptest suite's 512-case fuzz).
        if length == 0 {
            continue;
        }
        // Offset: signed LE with sign extension.
        let (offset, has_offset) =
            read_le_i64_signed(bytes, &mut cursor, offset_width).ok_or(RunError::Overrun)?;
        if i128::from(length) > remaining.max(0) {
            // Run longer than the VCN span: corrupt; skip it.
            continue;
        }
        remaining -= i128::from(length);
        let sparse = !has_offset && length > 0;
        let run_lcn = if sparse {
            None
        } else {
            match lcn.checked_add(offset) {
                Some(v) => {
                    lcn = v;
                    Some(v)
                }
                None => continue, // Overflow: corrupt delta, drop the run.
            }
        };
        out.push(RunEntry {
            length,
            lcn: run_lcn,
            first_vcn,
        });
    }
    if remaining > 0 {
        // Trailing hole = sparse tail. A span that does not fit u64
        // (corrupt first/last VCN pair — e.g. 0..u64::MAX) used to push
        // a PHANTOM length-0 run via `unwrap_or(0)`; skip the tail
        // instead (the run list simply does not cover the claim).
        if let Ok(len) = u64::try_from(remaining) {
            out.push(RunEntry {
                length: len,
                lcn: None,
                first_vcn,
            });
        }
    }
    Ok(out)
}

/// Read `width` little-endian bytes as u64 (width ≤ 8).
fn read_le_u64(bytes: &[u8], cursor: &mut usize, width: u8) -> Option<u64> {
    if width == 0 {
        return Some(0);
    }
    let end = cursor.checked_add(usize::from(width))?;
    if end > bytes.len() {
        return None;
    }
    let mut buf = [0u8; 8];
    buf[..usize::from(width)].copy_from_slice(&bytes[*cursor..end]);
    *cursor = end;
    Some(u64::from_le_bytes(buf))
}

/// Read `width` little-endian bytes as sign-extended i64. The second
/// return is false when the width is 0 (sparse marker).
fn read_le_i64_signed(bytes: &[u8], cursor: &mut usize, width: u8) -> Option<(i64, bool)> {
    if width == 0 {
        return Some((0, false));
    }
    let end = cursor.checked_add(usize::from(width))?;
    if end > bytes.len() {
        return None;
    }
    let mut buf = [0u8; 8];
    let slice = &bytes[*cursor..end];
    buf[..usize::from(width)].copy_from_slice(slice);
    // Sign-extend when the high bit of the last byte is set.
    if slice[usize::from(width) - 1] & 0x80 != 0 {
        for b in &mut buf[usize::from(width)..] {
            *b = 0xFF;
        }
    }
    *cursor = end;
    Some((i64::from_le_bytes(buf), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_two_runs() {
        // header 0x21: length 1 byte, offset 2 bytes (signed).
        // Run 1: length 5, delta +0x100. Run 2: length 3, delta +0x40.
        let bytes: Vec<u8> = vec![
            0x21, 0x05, 0x00, 0x01, // len 5, delta 256
            0x21, 0x03, 0x40, 0x00, // len 3, delta 64
            0x00,
        ];
        let runs = decode_run_list(&bytes, 0, bytes.len(), 0, 7).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].length, 5);
        assert_eq!(runs[0].lcn, Some(256));
        assert_eq!(runs[1].length, 3);
        assert_eq!(runs[1].lcn, Some(256 + 64));
    }

    #[test]
    fn negative_delta_goes_backwards() {
        // First run at +0x40, second with delta -0x10 (sign-extended 1 byte).
        let bytes: Vec<u8> = vec![
            0x11, 0x04, 0x40, // len 4, delta 64 (1-byte offset)
            0x11, 0x02, 0xF0, // len 2, delta -16 (0xF0 sign-extends)
            0x00,
        ];
        let runs = decode_run_list(&bytes, 0, bytes.len(), 0, 5).unwrap();
        assert_eq!(runs[1].lcn, Some(64 - 16));
    }

    #[test]
    fn sparse_run_has_no_lcn() {
        // header 0x01: length 1 byte, NO offset field → sparse.
        let bytes: Vec<u8> = vec![0x01, 0x08, 0x00];
        let runs = decode_run_list(&bytes, 0, bytes.len(), 0, 7).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].lcn, None);
        assert_eq!(runs[0].length, 8);
    }

    #[test]
    fn trailing_hole_becomes_sparse() {
        // One run of 2 clusters but the VCN span says 5 → sparse tail of 3.
        let bytes: Vec<u8> = vec![0x11, 0x02, 0x10, 0x00];
        let runs = decode_run_list(&bytes, 0, bytes.len(), 0, 4).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].lcn, Some(16));
        assert_eq!(runs[1].lcn, None);
        assert_eq!(runs[1].length, 3);
    }

    #[test]
    fn terminator_stops() {
        let bytes: Vec<u8> = vec![0x11, 0x02, 0x10, 0x00, 0x11, 0x02, 0x10];
        let runs = decode_run_list(&bytes, 0, bytes.len(), 0, 1).unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn bad_widths_are_skipped_not_fatal() {
        // 0x99: both widths "9" (invalid) → skip 18 bytes to the terminator.
        let mut bytes = vec![0x99];
        bytes.extend(std::iter::repeat_n(0u8, 18));
        bytes.push(0x00);
        let runs = decode_run_list(&bytes, 0, bytes.len(), 0, 0).unwrap();
        // The VCN span 0..0 = one cluster of trailing hole (sparse) at
        // most — never a located run.
        assert!(runs.iter().all(|r| r.lcn.is_none()));
        assert!(runs.len() <= 1);
    }

    #[test]
    fn overrun_detected() {
        // Claims a 4-byte length but the buffer ends.
        let bytes: Vec<u8> = vec![0x40, 0x01];
        assert!(decode_run_list(&bytes, 0, bytes.len(), 0, 4).is_err());
    }
}
