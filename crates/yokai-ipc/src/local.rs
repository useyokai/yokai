use crate::*;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use yokai_platform::local_ipc::{Client, ImagePin, IoError, Pipe, PipeName, ProcessIdentity, replace_file};

pub fn read_control<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?.take(CONTROL_MAX as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > CONTROL_MAX {
        return Err(invalid("control record too large"));
    }
    decode(&bytes)
}

pub fn publish_endpoint(control: &Path, endpoint: &Endpoint) -> io::Result<()> {
    let data = encode(endpoint, CONTROL_MAX)?;
    let locator = control.join("endpoint.json");
    let temp = control.join(format!("endpoint-{}.tmp", endpoint.epoch.nonce));
    let history = control.join(format!("endpoint-{}.json", endpoint.epoch.nonce));
    let mut archive = OpenOptions::new().create_new(true).write(true).open(history)?;
    archive.write_all(&data)?;
    let mut next = OpenOptions::new().create_new(true).write(true).open(&temp)?;
    next.write_all(&data)?;
    drop(next);
    replace_file(&temp, &locator)
}

pub fn nonce() -> io::Result<String> {
    yokai_platform::random_instance_id().map(|id| id.to_hex()).map_err(io::Error::other)
}

pub fn read_frame<T: DeserializeOwned>(pipe: &Pipe, cap: usize, first: Instant, absolute: Instant) -> Result<T, IoError> {
    let mut header = [0; 4];
    pipe.read_exact(&mut header[..1], first.min(absolute))?;
    let deadline = absolute.min(Instant::now() + Duration::from_secs(2));
    pipe.read_exact(&mut header[1..], deadline)?;
    let mut body = vec![0; length(header, cap)?];
    #[cfg(feature = "test-api")]
    FRAME_HIGH_WATER.fetch_max(body.capacity(), std::sync::atomic::Ordering::Relaxed);
    pipe.read_exact(&mut body, deadline)?;
    Ok(decode(&body)?)
}

pub fn write_frame<T: Serialize>(pipe: &Pipe, value: &T, cap: usize, deadline: Instant) -> Result<(), IoError> {
    let bytes = encode(value, cap)?;
    pipe.write_all(&(bytes.len() as u32).to_le_bytes(), deadline)?;
    pipe.write_all(&bytes, deadline)
}

pub struct Remote {
    transport: Client,
    binding: Binding,
    next_send: u64,
    next_recv: u64,
}

impl Remote {
    pub fn connect(endpoint: &Endpoint, config: &Enrollment) -> Result<Self, IoError> {
        if endpoint.epoch.instance != config.instance || endpoint.epoch.version != VERSION
            || endpoint.epoch.runtime_epoch < 1 || endpoint.epoch.instance.parse::<yokai_types::InstanceId>().is_err()
        {
            return Err(invalid("stale or incompatible endpoint").into());
        }
        let pin = ImagePin::open(&config.peer_image, &config.peer_sha256)?;
        let name = PipeName::from_nonce(&endpoint.epoch.nonce)?;
        let expected = ProcessIdentity { pid: endpoint.pid, creation_time: endpoint.creation_time };
        let transport = Client::connect(&name, &pin, expected)?;
        let hello = Hello { epoch: endpoint.epoch.clone(), client_nonce: nonce()? };
        let end = Instant::now() + Duration::from_secs(3);
        write_frame(&transport.pipe, &hello, CONTROL_MAX, end)?;
        let ready: Ready = read_frame(&transport.pipe, CONTROL_MAX, end, end)?;
        if ready.hello.epoch != hello.epoch || ready.hello.client_nonce != hello.client_nonce {
            return Err(invalid("stale handshake").into());
        }
        PipeName::from_nonce(&ready.session)?;
        let binding = Binding { epoch: hello.epoch, session: ready.session };
        Ok(Self { transport, binding, next_send: 1, next_recv: 1 })
    }

    pub fn send(&mut self, request: Request) -> Result<u64, IoError> {
        self.transport.peer.check_alive()?;
        let id = self.next_send;
        self.next_send = id.checked_add(1).ok_or_else(|| invalid("sequence exhausted"))?;
        let frame = Frame { binding: self.binding.clone(), sequence: id, request: id, body: request };
        write_frame(&self.transport.pipe, &frame, REQUEST_MAX, Instant::now() + Duration::from_secs(2))?;
        Ok(id)
    }

    pub fn receive(&mut self, end: Instant) -> Result<Frame<Reply>, IoError> {
        // A queued final frame may remain readable briefly after the peer exits.
        let reply: Frame<Reply> = read_frame(&self.transport.pipe, RESPONSE_MAX, end, end)?;
        if reply.binding != self.binding || reply.sequence != self.next_recv
            || reply.request == 0 || reply.request >= self.next_send
        {
            return Err(invalid("unexpected reply binding").into());
        }
        self.next_recv = self.next_recv.checked_add(1).ok_or_else(|| invalid("sequence exhausted"))?;
        Ok(reply)
    }
}

#[cfg(feature = "test-api")]
static FRAME_HIGH_WATER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "test-api")]
pub fn frame_high_water() -> usize { FRAME_HIGH_WATER.load(std::sync::atomic::Ordering::Relaxed) }
