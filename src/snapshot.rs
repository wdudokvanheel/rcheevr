//! Helpers for feeding RA condition evaluation from a flat memory snapshot.
//!
//! RA addresses are console-relative: GB targets `0x0000`–`0xFFFF`,
//! GBA targets `0x02000000`–`0x0203FFFF`, SNES targets `0x7E0000`–`0x7FFFFF`,
//! NES targets `0x0000`–`0x07FF`. Frontends (the wasm crate, the iOS FFI,
//! future native shells) all need the same logic to translate an RA-relative
//! address into a snapshot-buffer offset and assemble multi-byte values
//! according to [`MemSize`]. Centralising it here keeps that mapping in one
//! place; per-system layout bugs no longer fail silently in five different
//! callers.

use crate::memaddr::MemSize;

/// Number of bytes the given [`MemSize`] consumes from the underlying
/// snapshot. Bit/nibble sizes touch a single byte; word and three-byte sizes
/// touch 2 / 3 bytes; everything else (DWord, float, double) touches 4.
pub fn byte_count(size: MemSize) -> u32 {
    match size {
        MemSize::Bit0
        | MemSize::Bit1
        | MemSize::Bit2
        | MemSize::Bit3
        | MemSize::Bit4
        | MemSize::Bit5
        | MemSize::Bit6
        | MemSize::Bit7
        | MemSize::LowNibble
        | MemSize::HighNibble
        | MemSize::Byte
        | MemSize::BitCount => 1,
        MemSize::Word | MemSize::BigEndianWord => 2,
        MemSize::ThreeBytes | MemSize::BigEndianThreeBytes => 3,
        _ => 4,
    }
}

fn is_big_endian(size: MemSize) -> bool {
    matches!(
        size,
        MemSize::BigEndianWord
            | MemSize::BigEndianThreeBytes
            | MemSize::BigEndianDWord
            | MemSize::FloatBigEndian
            | MemSize::Double32BigEndian
    )
}

/// Read a value of `size` from `snapshot` at the RA-relative `addr`, treating
/// the snapshot as a contiguous mapping starting at `base_addr`.
///
/// Multi-byte values are assembled little-endian for the standard sizes and
/// big-endian for the explicit `BigEndian*` variants. Out-of-bounds reads
/// (below the base, or past the end of the buffer) produce `0` — matching
/// what RA expects when an address falls outside mapped RAM.
pub fn read_snapshot(snapshot: &[u8], addr: u32, size: MemSize, base_addr: u32) -> u32 {
    let bytes = byte_count(size);
    let big_endian = is_big_endian(size);
    let mut val: u32 = 0;
    for i in 0..bytes {
        let abs = addr.wrapping_add(i);
        if abs < base_addr {
            continue;
        }
        let off = (abs - base_addr) as usize;
        if off >= snapshot.len() {
            continue;
        }
        let byte = snapshot[off] as u32;
        let shift = if big_endian {
            8 * (bytes - 1 - i)
        } else {
            8 * i
        };
        val |= byte << shift;
    }
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_reads_single_byte() {
        let buf = [0u8, 0x42, 0u8, 0u8];
        assert_eq!(read_snapshot(&buf, 0x01, MemSize::Byte, 0), 0x42);
    }

    #[test]
    fn word_is_little_endian() {
        let buf = [0u8, 0x34, 0x12, 0u8];
        assert_eq!(read_snapshot(&buf, 0x01, MemSize::Word, 0), 0x1234);
    }

    #[test]
    fn big_endian_word_swaps_bytes() {
        let buf = [0u8, 0x12, 0x34, 0u8];
        assert_eq!(read_snapshot(&buf, 0x01, MemSize::BigEndianWord, 0), 0x1234);
    }

    #[test]
    fn three_bytes_little_endian() {
        let buf = [0u8, 0xCD, 0xAB, 0x12];
        assert_eq!(read_snapshot(&buf, 0x01, MemSize::ThreeBytes, 0), 0x12_ABCD);
    }

    #[test]
    fn dword_assembles_four_bytes_little_endian() {
        let buf = [0x78, 0x56, 0x34, 0x12];
        assert_eq!(read_snapshot(&buf, 0x00, MemSize::DWord, 0), 0x12345678);
    }

    #[test]
    fn base_addr_offsets_into_buffer() {
        // Simulate SNES WRAM: snapshot covers 0x7E0000..0x7E0010, base = 0x7E0000.
        let buf: Vec<u8> = (0..0x10).collect();
        // Read `MemSize::Byte` from RA addr 0x7E0005 → offset 5 → 0x05.
        assert_eq!(
            read_snapshot(&buf, 0x7E_0005, MemSize::Byte, 0x7E_0000),
            0x05
        );
    }

    #[test]
    fn out_of_range_addresses_read_as_zero() {
        let buf = [0xFFu8; 4];
        // Address well past the buffer.
        assert_eq!(read_snapshot(&buf, 0x10_000, MemSize::Byte, 0), 0);
        // Address below the base.
        assert_eq!(
            read_snapshot(&buf, 0x00, MemSize::Byte, 0x7E_0000),
            0
        );
    }

    #[test]
    fn partial_oob_word_zero_fills_missing_bytes() {
        // Buffer has only one byte at offset 0, the second byte of a Word
        // read falls past the end and reads as 0.
        let buf = [0x42u8];
        assert_eq!(read_snapshot(&buf, 0x00, MemSize::Word, 0), 0x0042);
    }

    #[test]
    fn matches_wasm_peek_snapshot_behaviour() {
        // Reproduces the exact assembly the wasm crate's `peek_snapshot`
        // performs: 64 KB GB snapshot, base 0, little-endian word read.
        let mut buf = vec![0u8; 0x1_0000];
        buf[0xC000] = 0xAA;
        buf[0xC001] = 0xBB;
        assert_eq!(read_snapshot(&buf, 0xC000, MemSize::Word, 0), 0xBBAA);
    }
}
