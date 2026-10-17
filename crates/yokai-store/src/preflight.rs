//! Read-only compatibility preflight and fixed-name inventory.
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
use crate::{IncompleteReason, InstanceError};

const NAMES: [&str; 4] = ["instance.db", "instance.db-wal", "instance.db-shm", "instance.db-journal"];
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryEntry {
    pub name: String,
    pub exists: bool,
    pub entry_type: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInventory {
    pub entries: Vec<InventoryEntry>,
}
impl FileInventory {
    pub fn describe_difference(&self, after: &Self) -> String {
        let mut changed = Vec::new();
        for before in &self.entries {
            let next = after.entries.iter().find(|entry| entry.name == before.name);
            if next != Some(before) {
                changed.push(format!("{}: {:?} -> {:?}", before.name, before, next));
            }
        }
        if changed.is_empty() { "no inventory changes".into() } else { changed.join("; ") }
    }
    fn same_names(&self, other: &Self, names: &[&str]) -> bool {
        names.iter().all(|name| {
            self.entries.iter().find(|e| e.name == *name)
                == other.entries.iter().find(|e| e.name == *name)
        })
    }
}

pub(crate) struct AcceptedPreflight {
    pub wal_present: bool,
}

pub fn capture_inventory(root: &Path) -> io::Result<FileInventory> {
    let mut entries = Vec::with_capacity(NAMES.len());
    for name in NAMES {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                yokai_platform::location::validate_regular_single_link(&path, &meta)?;
                let digest = hash_file(&path)?;
                entries.push(InventoryEntry {
                    name: name.to_owned(),
                    exists: true,
                    entry_type: Some("regular".to_owned()),
                    size: meta.len(),
                    sha256: Some(digest),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => entries.push(InventoryEntry {
                name: name.to_owned(),
                exists: false,
                entry_type: None,
                size: 0,
                sha256: None,
            }),
            Err(error) => return Err(error),
        }
    }
    if !entries[0].exists {
        return Err(io::Error::new(io::ErrorKind::NotFound, "instance.db is missing"));
    }
    Ok(FileInventory { entries })
}

pub(crate) fn stage_a(root: &Path, id: yokai_types::InstanceId, created: &str,plan:crate::schema::Plan) -> Result<AcceptedPreflight, InstanceError> {
    let before = capture_inventory(root)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound { InstanceError::Incomplete(IncompleteReason::NoDatabase) }
            else { InstanceError::PreflightUnavailable(e.to_string()) }
        })?;
    #[cfg(feature = "test-api")]
    crate::failpoint::mutate_stage_a_once(&root.join("instance.db"))
        .map_err(|e| InstanceError::PreflightUnavailable(format!("test Stage-A mutation hook: {e}")))?;
    let entry = |name: &str| before.entries.iter().find(|item| item.name == name).is_some_and(|item| item.exists);
    let wal_present = entry("instance.db-wal");
    let result = (|| -> Result<(), InstanceError> {
        let size = before.entries[0].size;
        let (header_version, page_size) = validate_header(&root.join("instance.db"), size)?;
        if entry("instance.db-journal") { return Err(InstanceError::UnexpectedJournal); }
        if !wal_present && entry("instance.db-shm") {
            return Err(InstanceError::SideFileStateUnverified { header_version });
        }
        if size == 0 && wal_present {
            return Err(InstanceError::SchemaInconsistent("zero-length database has a WAL side file".into()));
        }
        if wal_present {
            validate_wal_header(&root.join("instance.db-wal"), page_size)?;
            read_only_sqlite_check(&root.join("instance.db"), id, created,plan)
        } else if header_version > plan.latest() {
            Err(InstanceError::SchemaTooNew { found: header_version, maximum: plan.latest() })
        } else {
            Ok(())
        }
    })();
    let after = capture_inventory(root)
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    verify_stage_a_inventory(&before, &after, wal_present)?;
    result?;
    Ok(AcceptedPreflight { wal_present })
}
fn verify_stage_a_inventory(before: &FileInventory, after: &FileInventory, wal: bool) -> Result<(), InstanceError> {
    let core = ["instance.db", "instance.db-wal", "instance.db-journal"];
    if !before.same_names(after, &core) {
        return Err(InstanceError::PreflightWrote(before.describe_difference(after)));
    }
    if !wal && !before.same_names(after, &["instance.db-shm"]) {
        return Err(InstanceError::PreflightWrote(before.describe_difference(after)));
    }
    Ok(())
}

fn validate_header(path: &Path, file_len: u64) -> Result<(u32, u64), InstanceError> {
    if file_len == 0 { return Ok((0, 0)); }
    if file_len < 100 { return Err(InstanceError::HeaderInvalid("header shorter than 100 bytes".into())); }
    let mut header = [0u8; 100];
    File::open(path).and_then(|mut file| file.read_exact(&mut header))
        .map_err(|e| InstanceError::PreflightUnavailable(format!("read SQLite header: {e}")))?;
    if &header[..16] != SQLITE_MAGIC { return Err(InstanceError::HeaderInvalid("magic".into())); }
    let raw_page = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if raw_page == 1 { 65_536u64 } else { raw_page as u64 };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(InstanceError::HeaderInvalid("page size".into()));
    }
    if header[18] == 1 || header[19] == 1 { return Err(InstanceError::UnexpectedJournal); }
    if header[18] != 2 || header[19] != 2 {
        return Err(InstanceError::HeaderInvalid("read/write format versions".into()));
    }
    if header[20] != 0 { return Err(InstanceError::HeaderInvalid("reserved bytes".into())); }
    if header[21..24] != [64, 32, 32] { return Err(InstanceError::HeaderInvalid("payload fractions".into())); }
    if file_len == 0 || file_len % page_size != 0 {
        return Err(InstanceError::HeaderInvalid("database length".into()));
    }
    let schema_format = u32::from_be_bytes(header[44..48].try_into().expect("fixed slice"));
    if schema_format > 4 { return Err(InstanceError::HeaderInvalid("schema format".into())); }
    let encoding = u32::from_be_bytes(header[56..60].try_into().expect("fixed slice"));
    // SQLite leaves the encoding unset while the database has no schema.
    if encoding != 1 && !(schema_format == 0 && encoding == 0) {
        return Err(InstanceError::HeaderInvalid("text encoding".into()));
    }
    Ok((u32::from_be_bytes(header[60..64].try_into().expect("fixed slice")), page_size))
}

fn validate_wal_header(path: &Path, database_page_size: u64) -> Result<(), InstanceError> {
    let mut file = File::open(path)
        .map_err(|e| InstanceError::PreflightUnavailable(format!("open WAL header: {e}")))?;
    let length = file.metadata()
        .map_err(|e| InstanceError::PreflightUnavailable(format!("stat WAL: {e}")))?.len();
    if length == 0 { return Ok(()); }
    if length < 32 {
        return Err(InstanceError::SchemaInconsistent("WAL header is shorter than 32 bytes".into()));
    }
    let mut header = [0u8; 32];
    file.read_exact(&mut header)
        .map_err(|e| InstanceError::PreflightUnavailable(format!("read WAL header: {e}")))?;
    let magic = u32::from_be_bytes(header[0..4].try_into().expect("fixed slice"));
    let big_endian_checksum = match magic {
        0x377f0682 => false,
        0x377f0683 => true,
        _ => return Err(InstanceError::SchemaInconsistent("invalid WAL magic".into())),
    };
    if u32::from_be_bytes(header[4..8].try_into().expect("fixed slice")) != 3_007_000 {
        return Err(InstanceError::SchemaInconsistent("unsupported WAL format version".into()));
    }
    if u64::from(u32::from_be_bytes(header[8..12].try_into().expect("fixed slice"))) != database_page_size {
        return Err(InstanceError::SchemaInconsistent("WAL page size differs from database".into()));
    }
    let checksum = wal_checksum(&header[..24], big_endian_checksum)
        .ok_or_else(|| InstanceError::SchemaInconsistent("invalid WAL header checksum input".into()))?;
    let stored = (
        u32::from_be_bytes(header[24..28].try_into().expect("fixed slice")),
        u32::from_be_bytes(header[28..32].try_into().expect("fixed slice")),
    );
    if checksum != stored {
        return Err(InstanceError::SchemaInconsistent("WAL header checksum mismatch".into()));
    }
    Ok(())
}

fn wal_checksum(bytes: &[u8], big_endian: bool) -> Option<(u32, u32)> {
    if bytes.len() % 8 != 0 { return None; }
    let mut s0 = 0u32;
    let mut s1 = 0u32;
    for pair in bytes.chunks_exact(8) {
        let x0 = if big_endian {
            u32::from_be_bytes(pair[..4].try_into().ok()?)
        } else {
            u32::from_le_bytes(pair[..4].try_into().ok()?)
        };
        let x1 = if big_endian {
            u32::from_be_bytes(pair[4..8].try_into().ok()?)
        } else {
            u32::from_le_bytes(pair[4..8].try_into().ok()?)
        };
        s0 = s0.wrapping_add(x0).wrapping_add(s1);
        s1 = s1.wrapping_add(x1).wrapping_add(s0);
    }
    Some((s0, s1))
}

fn read_only_sqlite_check(path: &Path, id: yokai_types::InstanceId, created: &str,plan:crate::schema::Plan) -> Result<(), InstanceError> {
    #[cfg(feature = "test-api")]
    crate::failpoint::record_connection("stage-a-read-only");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut opened = Connection::open_with_flags(path, flags)
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    opened.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    opened.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    opened.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    // Pin all compatibility queries to one consistent SQLite read snapshot.
    let tx=opened.transaction().map_err(|e|InstanceError::PreflightUnavailable(e.to_string()))?;
    let result = crate::schema::inspect_read_only(&tx,id,created,plan).map(|_| ());
    tx.rollback().map_err(|e|InstanceError::PreflightUnavailable(e.to_string()))?;
    drop(opened);
    result
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hash = Sha256::new(); hash.update(bytes); hash.finish()
}

#[cfg(feature = "test-api")]
pub(crate) fn sha256_chunks_for_test(chunks: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for chunk in chunks { hash.update(chunk); }
    hash.finish()
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finish())
}

/// Small streaming SHA-256 implementation for inventories and migration drift detection.
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}
impl Sha256 {
    fn new() -> Self {
        Self {
            state: [0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19],
            buffer: [0;64], buffered: 0, length: 0,
        }
    }
    fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let block: &[u8;64] = data[..64].try_into().expect("fixed block");
            self.compress(block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }
    fn finish(mut self) -> String {
        let bit_length = self.length.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0;64];
            self.buffered = 0;
        }
        self.buffer[self.buffered..56].fill(0);
        self.buffer[56..64].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut out = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write as _;
            let _ = write!(out, "{word:08x}");
        }
        out
    }
    fn compress(&mut self, block: &[u8;64]) {
        const K: [u32;64] = [
            0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
            0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
            0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
            0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
            0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
            0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
            0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
            0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2,
        ];
        let mut w = [0u32;64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i*4..i*4+4].try_into().expect("word"));
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16].wrapping_add(s0).wrapping_add(w[i-7]).wrapping_add(s1);
        }
        let [mut a,mut b,mut c,mut d,mut e,mut f,mut g,mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h=g; g=f; f=e; e=d.wrapping_add(t1); d=c; c=b; b=a; a=t1.wrapping_add(t2);
        }
        self.state[0]=self.state[0].wrapping_add(a);
        self.state[1]=self.state[1].wrapping_add(b);
        self.state[2]=self.state[2].wrapping_add(c);
        self.state[3]=self.state[3].wrapping_add(d);
        self.state[4]=self.state[4].wrapping_add(e);
        self.state[5]=self.state[5].wrapping_add(f);
        self.state[6]=self.state[6].wrapping_add(g);
        self.state[7]=self.state[7].wrapping_add(h);
    }
}
