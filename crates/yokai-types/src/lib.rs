//! Identifiers shared across Yokai crates. This crate performs no I/O and has no dependencies.
#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;

mod conversation;
pub use conversation::*;

/// Identity of one Yokai Instance: 16 bytes chosen when the Instance is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId([u8; 16]);

impl InstanceId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self { Self(bytes) }
    pub const fn as_bytes(&self) -> &[u8; 16] { &self.0 }

    /// Lowercase hexadecimal without separators, used for the Instance directory and identity file.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

impl FromStr for InstanceId {
    type Err = InvalidId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 36
            || ![8, 13, 18, 23].iter().all(|&i| value.as_bytes().get(i) == Some(&b'-'))
        {
            return Err(InvalidId { kind: "InstanceId" });
        }
        let mut bytes = [0u8; 16];
        let mut nibble = 0usize;
        let mut high = None;
        for (index, byte) in value.bytes().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) { continue; }
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => return Err(InvalidId { kind: "InstanceId" }),
            };
            if let Some(hi) = high.take() {
                bytes[nibble / 2] = (hi << 4) | value;
                nibble += 2;
            } else {
                high = Some(value);
            }
        }
        if nibble != 32 || high.is_some() {
            return Err(InvalidId { kind: "InstanceId" });
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) { f.write_str("-")?; }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Client-chosen identity of a command, used to recognize a resubmitted request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(String);

impl CommandId {
    pub const MAX_LEN: usize = 128;
    pub fn new(value: &str) -> Result<Self, InvalidId> {
        if value.is_empty() || value.len() > Self::MAX_LEN {
            return Err(InvalidId { kind: "CommandId" });
        }
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidId { kind: &'static str }

impl InvalidId {
    pub fn kind(&self) -> &'static str { self.kind }
}
impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "invalid {}", self.kind) }
}
impl std::error::Error for InvalidId {}
