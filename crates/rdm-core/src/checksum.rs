//! Verifying a finished file against a hash the user supplied.

use std::io::{self, Read};
use std::path::Path;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha256,
}

impl Algo {
    pub fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "MD5",
            Algo::Sha1 => "SHA-1",
            Algo::Sha256 => "SHA-256",
        }
    }
}

/// Accepts a bare hex digest (the algorithm follows from its length) or
/// `md5:…` / `sha1:…` / `sha256:…`. Returns the algorithm and lower-case hex.
pub fn parse(text: &str) -> Option<(Algo, String)> {
    let text = text.trim();
    let hex_part = match text.split_once(':') {
        Some((prefix, rest)) => {
            let wanted = match prefix.trim().to_ascii_lowercase().replace('-', "").as_str() {
                "md5" => Algo::Md5,
                "sha1" => Algo::Sha1,
                "sha256" => Algo::Sha256,
                _ => return None,
            };
            let (algo, hex) = parse(rest)?;
            return (algo == wanted).then_some((algo, hex));
        }
        None => text,
    };
    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let algo = match hex_part.len() {
        32 => Algo::Md5,
        40 => Algo::Sha1,
        64 => Algo::Sha256,
        _ => return None,
    };
    Some((algo, hex_part.to_ascii_lowercase()))
}

pub fn hash_file(path: &Path, algo: Algo) -> io::Result<String> {
    fn run<D: Digest>(path: &Path) -> io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = D::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }
    match algo {
        Algo::Md5 => run::<Md5>(path),
        Algo::Sha1 => run::<Sha1>(path),
        Algo::Sha256 => run::<Sha256>(path),
    }
}

/// `Ok(())` when the file matches; otherwise a message fit for the user.
pub fn verify(path: &Path, expected: &str) -> Result<(), String> {
    let (algo, want) = parse(expected).ok_or_else(|| "the expected checksum is not a valid MD5, SHA-1 or SHA-256 value".to_string())?;
    let got = hash_file(path, algo).map_err(|e| format!("cannot read the file to verify it: {e}"))?;
    if got == want {
        Ok(())
    } else {
        Err(format!("{} mismatch: expected {want}, the file has {got}", algo.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_by_length_and_prefix() {
        assert_eq!(parse("D41D8CD98F00B204E9800998ECF8427E"), Some((Algo::Md5, "d41d8cd98f00b204e9800998ecf8427e".into())));
        assert_eq!(parse(" sha1: da39a3ee5e6b4b0d3255bfef95601890afd80709 ").map(|p| p.0), Some(Algo::Sha1));
        assert_eq!(parse(&"a".repeat(64)).map(|p| p.0), Some(Algo::Sha256));
        assert_eq!(parse("SHA-256:".to_string().as_str()), None);
        assert_eq!(parse(&format!("md5:{}", "a".repeat(64))), None, "prefix and length disagree");
        assert_eq!(parse("not-a-hash"), None);
        assert_eq!(parse(&"g".repeat(32)), None);
    }

    #[test]
    fn hashes_known_content() {
        let dir = std::env::temp_dir().join(format!("rdm-sum-{}-{}", std::process::id(), fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.txt");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(hash_file(&path, Algo::Md5).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hash_file(&path, Algo::Sha1).unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hash_file(&path, Algo::Sha256).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(verify(&path, "900150983CD24FB0D6963F7D28E17F72").is_ok());
        let err = verify(&path, &"0".repeat(64)).unwrap_err();
        assert!(err.starts_with("SHA-256 mismatch"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
