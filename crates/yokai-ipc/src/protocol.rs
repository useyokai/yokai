use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Write};

pub const VERSION: u32 = 1;
pub const CONTROL_MAX: usize = 4096;
pub const REQUEST_MAX: usize = 128 * 1024;
pub const RESPONSE_MAX: usize = 2 * 1024 * 1024;

pub fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn length(header: [u8; 4], limit: usize) -> io::Result<usize> {
    let count = u32::from_le_bytes(header) as usize;
    if !(1..=limit).contains(&count) {
        return Err(invalid("frame length outside bounds"));
    }
    Ok(count)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|_| invalid("invalid protocol JSON"))
}

pub fn encode<T: Serialize>(value: &T, limit: usize) -> io::Result<Vec<u8>> {
    struct Capped {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit - self.bytes.len() {
                return Err(invalid("encoded value exceeds limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    // Reserve once, with an exact cap rather than geometric Vec growth past the limit.
    let mut writer = Capped { bytes: Vec::with_capacity(limit), limit };
    serde_json::to_writer(&mut writer, value).map_err(|_| invalid("cannot encode bounded value"))?;
    Ok(writer.bytes)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Epoch {
    pub version: u32,
    pub instance: String,
    pub runtime_epoch: i64,
    pub nonce: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub epoch: Epoch,
    pub pid: u32,
    pub creation_time: u64,
    pub build: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub peer_image: std::path::PathBuf,
    pub peer_sha256: String,
    pub instance: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub epoch: Epoch,
    pub client_nonce: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub hello: Hello,
    pub session: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub epoch: Epoch,
    pub session: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frame<T> {
    pub binding: Binding,
    pub sequence: u64,
    pub request: u64,
    pub body: T,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    #[serde(deserialize_with = "deserialize_empty_request")]
    Status,
    Create { command: String, conversation: String },
    Submit { command: String, conversation: String, message: String, text: String },
    Read { conversation: String },
    Replay { instance: String, conversation: String, after: i64, limit: usize },
    #[serde(deserialize_with = "deserialize_empty_request")]
    Shutdown,
}

// Serde's internally tagged unit visitor discards extra fields. Decode an empty
// struct instead so these requests enforce the same strict field policy.
fn deserialize_empty_request<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}

    Empty::deserialize(deserializer).map(|_| ())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Open,
    Held,
    Completed,
    Failed,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub sequence: i64,
    pub run: i64,
    pub kind: String,
    pub message: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Status { lifecycle: String, store: String },
    Decision { code: String, run: Option<i64>, state: Option<RunStatus> },
    Progress { run: i64, index: usize, text: String },
    State { found: bool, latest: Option<(i64, RunStatus)> },
    Replay { events: Vec<Event>, after: i64, has_more: bool },
    Error { code: String, certainty: Option<String>, phase: Option<String> },
    ShutdownAccepted,
    Closed,
}

impl Reply {
    pub fn error(code: &str) -> Self {
        Self::Error { code: code.into(), certainty: None, phase: None }
    }
}
