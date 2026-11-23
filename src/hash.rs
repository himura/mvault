//! SHA-256 digests, the identity of every blob.

use std::fmt;
use std::str::FromStr;

use sha2::Digest as _;

/// A SHA-256 digest.
///
/// Parsed from and displayed as 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    /// Wraps the raw bytes of a digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes of the digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The digest of `data`.
    #[must_use]
    pub fn of(data: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(data);
        hasher.finish()
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

/// The error returned when a string is not a SHA-256 digest in hexadecimal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a sha256 digest (64 hexadecimal characters): {0:?}")]
pub struct ParseSha256Error(String);

impl FromStr for Sha256 {
    type Err = ParseSha256Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseSha256Error(s.to_owned());
        let digits = s.as_bytes();
        if digits.len() != 64 {
            return Err(err());
        }
        let mut bytes = [0_u8; 32];
        let (pairs, _) = digits.as_chunks::<2>();
        for (byte, [hi, lo]) in bytes.iter_mut().zip(pairs) {
            let hi = hex_value(*hi).ok_or_else(err)?;
            let lo = hex_value(*lo).ok_or_else(err)?;
            *byte = hi << 4 | lo;
        }
        Ok(Self(bytes))
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Computes a [`Sha256`] incrementally.
#[derive(Clone, Default)]
pub struct Hasher(sha2::Sha256);

impl Hasher {
    /// A hasher that has seen no data.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds `data` into the hasher.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// The digest of everything fed so far.
    #[must_use]
    pub fn finish(self) -> Sha256 {
        Sha256(self.0.finalize().into())
    }
}

impl fmt::Debug for Hasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hasher").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hashes_known_inputs() {
        assert_eq!(Sha256::of(b"").to_string(), EMPTY);
        assert_eq!(Sha256::of(b"abc").to_string(), ABC);
    }

    #[test]
    fn incremental_updates_match_one_shot() {
        let mut hasher = Hasher::new();
        hasher.update(b"a");
        hasher.update(b"");
        hasher.update(b"bc");
        assert_eq!(hasher.finish(), Sha256::of(b"abc"));
    }

    #[test]
    fn parses_and_displays_lowercase() {
        let digest: Sha256 = ABC.to_uppercase().parse().unwrap();
        assert_eq!(digest, Sha256::of(b"abc"));
        assert_eq!(digest.to_string(), ABC);
        assert_eq!(format!("{digest:?}"), format!("Sha256({ABC})"));
    }

    #[test]
    fn rejects_malformed_digests() {
        for input in [
            "",
            "abc",
            &ABC[..63],
            &format!("{ABC}0"),
            &format!("{}g", &ABC[..63]),
            &format!("{}é", &ABC[..62]),
        ] {
            assert!(input.parse::<Sha256>().is_err(), "{input:?}");
        }
    }
}
