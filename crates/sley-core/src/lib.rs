use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const UPSTREAM_GIT_COMPAT_VERSION: &str = "2.54.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub const fn raw_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    pub const fn hex_len(self) -> usize {
        self.raw_len() * 2
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
}

impl FromStr for ObjectFormat {
    type Err = GitError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            other => Err(GitError::Unsupported(format!("object format {other}"))),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId {
    format: ObjectFormat,
    bytes: [u8; 32],
}

impl ObjectId {
    pub fn from_raw(format: ObjectFormat, raw: &[u8]) -> Result<Self> {
        if raw.len() != format.raw_len() {
            return Err(GitError::InvalidObjectId(format!(
                "expected {} bytes for {}, got {}",
                format.raw_len(),
                format.name(),
                raw.len()
            )));
        }
        let mut bytes = [0; 32];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self { format, bytes })
    }

    pub fn from_hex(format: ObjectFormat, hex: &str) -> Result<Self> {
        if hex.len() != format.hex_len() {
            return Err(GitError::InvalidObjectId(format!(
                "expected {} hex digits for {}, got {}",
                format.hex_len(),
                format.name(),
                hex.len()
            )));
        }
        let mut raw = [0; 32];
        for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            raw[i] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self { format, bytes: raw })
    }

    pub const fn format(&self) -> ObjectFormat {
        self.format
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.format.raw_len()]
    }

    pub fn to_hex(&self) -> String {
        to_hex(self.as_bytes())
    }

    /// The all-zero ("null") object id for `format`.
    pub fn null(format: ObjectFormat) -> Self {
        Self {
            format,
            bytes: [0; 32],
        }
    }

    /// True when every byte is zero (the null oid).
    pub fn is_null(&self) -> bool {
        self.as_bytes().iter().all(|byte| *byte == 0)
    }

    /// The id of the canonical empty tree for `format` (`4b825dc6…` for SHA-1).
    pub fn empty_tree(format: ObjectFormat) -> Self {
        Self::digest_object(format, "tree", b"")
    }

    /// The id of the canonical empty blob for `format` (`e69de29b…` for SHA-1).
    pub fn empty_blob(format: ObjectFormat) -> Self {
        Self::digest_object(format, "blob", b"")
    }

    /// Hash `"<type> <len>\0<body>"` straight into an id, bypassing the
    /// fallible length check in [`ObjectId::from_raw`] (our own digests are
    /// always the right length) so the well-known constants stay infallible.
    fn digest_object(format: ObjectFormat, object_type: &str, body: &[u8]) -> Self {
        let mut framed = Vec::with_capacity(object_type.len() + body.len() + 32);
        framed.extend_from_slice(object_type.as_bytes());
        framed.push(b' ');
        framed.extend_from_slice(body.len().to_string().as_bytes());
        framed.push(0);
        framed.extend_from_slice(body);
        let mut bytes = [0u8; 32];
        match format {
            ObjectFormat::Sha1 => bytes[..20].copy_from_slice(&sha1(&framed)),
            ObjectFormat::Sha256 => bytes[..32].copy_from_slice(&sha256(&framed)),
        }
        Self { format, bytes }
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObjectId").field(&self.to_hex()).finish()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for ObjectId {
    type Err = GitError;

    /// Parse a full hex id, inferring the hash from its length (40 hex digits =
    /// SHA-1, 64 = SHA-256).
    fn from_str(text: &str) -> Result<Self> {
        let format = match text.len() {
            40 => ObjectFormat::Sha1,
            64 => ObjectFormat::Sha256,
            other => {
                return Err(GitError::InvalidObjectId(format!(
                    "expected 40 or 64 hex digits, got {other}"
                )));
            }
        };
        Self::from_hex(format, text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteString(Vec<u8>);

impl ByteString {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<&str> for ByteString {
    fn from(value: &str) -> Self {
        Self(value.as_bytes().to_vec())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoPath(PathBuf);

impl RepoPath {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.is_absolute() {
            return Err(GitError::InvalidPath(
                "repository paths must be relative".into(),
            ));
        }
        if path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        }) {
            return Err(GitError::InvalidPath(
                "repository paths must not escape".into(),
            ));
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: ByteString,
    pub email: ByteString,
    pub time: GitTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitTime {
    pub seconds: i64,
    pub timezone_offset_minutes: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    Io(String),
    InvalidObjectId(String),
    InvalidObject(String),
    InvalidFormat(String),
    InvalidPath(String),
    Unsupported(String),
    NotFound(String),
    Transaction(String),
    Command(String),
    Exit(i32),
}

pub type Result<T> = std::result::Result<T, GitError>;

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "io error: {msg}"),
            Self::InvalidObjectId(msg) => write!(f, "invalid object id: {msg}"),
            Self::InvalidObject(msg) => write!(f, "invalid object: {msg}"),
            Self::InvalidFormat(msg) => write!(f, "invalid format: {msg}"),
            Self::InvalidPath(msg) => write!(f, "invalid path: {msg}"),
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::Transaction(msg) => write!(f, "transaction failed: {msg}"),
            Self::Command(msg) => write!(f, "command failed: {msg}"),
            Self::Exit(code) => write!(f, "exit {code}"),
        }
    }
}

impl Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

pub fn object_id_for_bytes(
    format: ObjectFormat,
    object_type: &str,
    body: &[u8],
) -> Result<ObjectId> {
    let mut framed = Vec::with_capacity(object_type.len() + body.len() + 32);
    framed.extend_from_slice(object_type.as_bytes());
    framed.push(b' ');
    framed.extend_from_slice(body.len().to_string().as_bytes());
    framed.push(0);
    framed.extend_from_slice(body);
    digest_bytes(format, &framed)
}

pub fn digest_bytes(format: ObjectFormat, bytes: &[u8]) -> Result<ObjectId> {
    match format {
        ObjectFormat::Sha1 => ObjectId::from_raw(format, &sha1(bytes)),
        ObjectFormat::Sha256 => ObjectId::from_raw(format, &sha256(bytes)),
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(GitError::InvalidObjectId(format!(
            "non-hex byte {:?}",
            byte as char
        ))),
    }
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xefcdab89;
    let mut h2: u32 = 0x98badcfe;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xc3d2e1f0;

    let bit_len = (input.len() as u64) * 8;
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let offset = i * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0; 20];
    out[..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (input.len() as u64) * 8;
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let offset = i * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0; 32];
    for (idx, word) in h.iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_blob_matches_git_known_value() {
        let oid = object_id_for_bytes(ObjectFormat::Sha1, "blob", b"hello\n").unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn sha256_blob_matches_git_known_value() {
        let oid = object_id_for_bytes(ObjectFormat::Sha256, "blob", b"hello\n").unwrap();
        assert_eq!(
            oid.to_hex(),
            "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
        );
    }

    #[test]
    fn object_id_round_trips_hex() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }
}
