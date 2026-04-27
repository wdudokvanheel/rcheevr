//! ROM hashing.
//!
//! GB: MD5 of the full ROM. GBA: MD5 of the full ROM. Per-system rules may
//! diverge later; keep helpers per-system rather than one generic function.

use md5::{Digest, Md5};

/// Compute the lowercase hex MD5 of `rom`.
fn md5_hex(rom: &[u8]) -> String {
    let digest = Md5::digest(rom);
    format!("{digest:x}")
}

/// Compute the lowercase hex MD5 of a UTF-8 string. Used by the API client
/// for the `awardachievement` / `submitlbentry` validation hashes.
pub(crate) fn md5_hex_str(s: &str) -> String {
    md5_hex(s.as_bytes())
}

/// Hash a Game Boy ROM for RetroAchievements lookup.
///
/// Returns a 32-character lowercase hex MD5 string.
pub fn hash_gb(rom: &[u8]) -> String {
    md5_hex(rom)
}

/// Hash a Game Boy Advance ROM for RetroAchievements lookup.
///
/// Returns a 32-character lowercase hex MD5 string.
pub fn hash_gba(rom: &[u8]) -> String {
    md5_hex(rom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slice_md5() {
        // MD5("") = d41d8cd98f00b204e9800998ecf8427e — well-known constant
        assert_eq!(hash_gb(&[]), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hash_gba(&[]), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn abc_md5() {
        // MD5("abc") = 900150983cd24fb0d6963f7d28e17f72
        assert_eq!(hash_gb(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hash_gba(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn hash_length_is_32() {
        let h = hash_gb(b"some rom bytes here");
        assert_eq!(h.len(), 32);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn gb_and_gba_agree_on_same_input() {
        let data = b"identical input bytes";
        assert_eq!(hash_gb(data), hash_gba(data));
    }

    #[test]
    fn known_hash_hello_world() {
        // MD5("Hello, World!") = 65a8e27d8879283831b664bd8b7f0ad4
        assert_eq!(
            hash_gb(b"Hello, World!"),
            "65a8e27d8879283831b664bd8b7f0ad4"
        );
    }
}
