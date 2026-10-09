//! Identifiers shared across Yokai crates. This crate performs no I/O and has no dependencies.
#![forbid(unsafe_code)]

use std::fmt;

/// Identity of one Yokai Instance: 16 bytes chosen when the Instance is created.
///
/// An Instance is identified by this value, never by its filesystem path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId([u8; 16]);

impl InstanceId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    /// Lower-case hexadecimal in 8-4-4-4-12 groups.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Client-chosen identity of a command, used to recognize a resubmitted request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(String);

impl CommandId {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 128;

    /// Accepts 1 to [`Self::MAX_LEN`] bytes.
    pub fn new(value: &str) -> Result<Self, InvalidId> {
        if value.is_empty() || value.len() > Self::MAX_LEN {
            return Err(InvalidId { kind: "CommandId" });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A value was rejected as an identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidId {
    kind: &'static str,
}

impl InvalidId {
    /// The identifier type that rejected the value.
    pub fn kind(&self) -> &'static str {
        self.kind
    }
}

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}", self.kind)
    }
}

impl std::error::Error for InvalidId {}
