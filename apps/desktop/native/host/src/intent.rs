use serde::{Deserialize, Serialize};
use std::{fs::{self, OpenOptions}, io::{Read, Write}, path::{Component, Path, PathBuf}};
use yokai_desktop_contract::MAX_TEXT_BYTES;
use yokai_platform::InstanceLock;

pub const RECORD_MAX: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Prepared, Creating, Created, Submitting, Accepted, Unknown, Held, Rejected }

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub create_command: Option<String>,
    pub submit_command: String,
    pub conversation: String,
    pub message: String,
    pub text: String,
    pub phase: Phase,
    pub accepted_run: Option<i64>,
}

impl Intent {
    pub fn ready_to_submit(&self) -> bool {
        self.accepted_run.is_none() && match &self.create_command {
            Some(_) => matches!(self.phase,Phase::Created),
            None => matches!(self.phase,Phase::Prepared),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub version: u32,
    pub instance: String,
    pub conversation: Option<String>,
    pub intent: Option<Intent>,
    pub draft: String,
}

pub fn id(value: &str) -> Result<(), String> {
    value.parse::<yokai_types::InstanceId>().map(|_| ()).map_err(|_| "Invalid identity.".into())
}

fn nonce() -> Result<String, String> {
    yokai_platform::random_instance_id().map(|v| v.to_string()).map_err(|_| "Identity generation failed.".into())
}

impl Record {
    pub fn empty(instance: String) -> Self {
        Self { version: 1, instance, conversation: None, intent: None, draft: String::new() }
    }

    pub fn validate(&self, instance: &str) -> Result<(), String> {
        id(&self.instance)?;
        if self.version != 1 || self.instance != instance || self.draft.len() > MAX_TEXT_BYTES {
            return Err("Invalid restoration record.".into());
        }
        if let Some(c) = &self.conversation { id(c)?; }
        if let Some(i) = &self.intent {
            id(&i.conversation)?; id(&i.message)?; id(&i.submit_command)?;
            if let Some(c) = &i.create_command { id(c)?; }
            if self.conversation.as_ref() != Some(&i.conversation) || i.text.trim().is_empty()
                || i.text.len() > MAX_TEXT_BYTES || i.accepted_run.is_some_and(|r| r <= 0)
                || (matches!(i.phase, Phase::Accepted | Phase::Held) && i.accepted_run.is_none())
                || (matches!(i.phase, Phase::Prepared | Phase::Creating | Phase::Created | Phase::Submitting | Phase::Rejected) && i.accepted_run.is_some())
                || (matches!(i.phase, Phase::Creating | Phase::Created) && i.create_command.is_none()) {
                return Err("Invalid restored intent.".into());
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8], instance: &str) -> Result<Self, String> {
        if bytes.len() > RECORD_MAX { return Err("Restoration record exceeds bound.".into()); }
        let mut value: Self = serde_json::from_slice(bytes).map_err(|_| "Invalid restoration JSON.")?;
        value.validate(instance)?;
        // Restart never resumes an interrupted send, even if its last saved phase was prepared.
        if let Some(i)=value.intent.as_mut() {
            if matches!(i.phase,Phase::Prepared|Phase::Creating|Phase::Created|Phase::Submitting) { i.phase=Phase::Unknown; }
        }
        Ok(value)
    }

    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        self.validate(&self.instance)?;
        yokai_ipc::encode(self, RECORD_MAX).map_err(|_| "Restoration record exceeds bound.".into())
    }

    pub fn begin(&mut self, text: String) -> Result<(), String> {
        if self.intent.is_some() || text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err("A new intent is not eligible.".into());
        }
        let create_command = if self.conversation.is_none() { Some(nonce()?) } else { None };
        let conversation = match &self.conversation { Some(c) => c.clone(), None => nonce()? };
        let intent = Intent { create_command, submit_command: nonce()?, conversation: conversation.clone(), message: nonce()?, text, phase: Phase::Prepared, accepted_run: None };
        self.conversation = Some(conversation);
        self.intent = Some(intent);
        self.draft.clear();
        self.bytes()?;
        Ok(())
    }
}

/// Fixed paths come from the sealed native launch, never from renderer input.
pub fn ordinary(path: &Path, directory: bool) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err("An absolute ordinary path is required.".into());
    }
    for p in path.ancestors() {
        let m = fs::symlink_metadata(p).map_err(|_| "Cannot inspect owned path.")?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if m.file_attributes() & 0x400 != 0 { return Err("Reparse path refused.".into()); }
        }
        if m.file_type().is_symlink() || (p != path && !m.is_dir()) {
            return Err("Unsafe path ancestor.".into());
        }
        if p == path && ((directory && !m.is_dir()) || (!directory && !m.is_file())) {
            return Err("Unexpected path kind.".into());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub struct Store {
    pub(crate) guard: std::sync::Arc<InstanceLock>,
    directory: PathBuf,
    record: PathBuf,
    temp: PathBuf,
    pub value: Record,
}

#[cfg(windows)]
pub(crate) fn acquire_guard(directory: &Path) -> Result<std::sync::Arc<InstanceLock>,String> {
    ordinary(directory,true)?;
    let guard=directory.join("desktop.guard");
    let lock=match fs::symlink_metadata(&guard) {
        Ok(_) => { ordinary(&guard,false)?; InstanceLock::open_and_lock(&guard) },
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => InstanceLock::create_and_lock(&guard),
        Err(_) => return Err("Cannot inspect ownership guard.".into()),
    }.map_err(|_| "Client ownership is unavailable.")?;
    Ok(std::sync::Arc::new(lock))
}

#[cfg(windows)]
impl Store {
    pub fn open(directory: &Path, instance: &str) -> Result<Self, String> {
        ordinary(directory, true)?; id(instance)?;
        let record = directory.join("intent.json");
        let temp = directory.join("intent.tmp");
        if fs::symlink_metadata(&temp).is_ok() { return Err("Retained temporary record; preserve it.".into()); }
        let guard = acquire_guard(directory)?;
        let value = match fs::symlink_metadata(&record) {
            Ok(_) => {
                ordinary(&record, false)?;
                let mut bytes = Vec::new();
                fs::File::open(&record).and_then(|f| f.take(RECORD_MAX as u64 + 1).read_to_end(&mut bytes)).map_err(|_| "Cannot read restoration record.")?;
                Record::decode(&bytes, instance)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::empty(instance.into()),
            Err(_) => return Err("Cannot inspect restoration record.".into()),
        };
        Ok(Self { guard, directory: directory.into(), record, temp, value })
    }

    /// Publish before dispatch. A leftover temp or failed replace is never removed.
    pub fn save(&mut self, value: Record) -> Result<(), String> {
        value.validate(&self.value.instance)?;
        let bytes = value.bytes()?;
        ordinary(&self.directory, true)?;
        match fs::symlink_metadata(&self.record) {
            Ok(_) => ordinary(&self.record, false)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("Cannot inspect record destination.".into()),
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(&self.temp).map_err(|_| "Temporary record unavailable; preserve it.")?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|_| "Record write incomplete; preserve it.")?;
        drop(file);
        yokai_platform::local_ipc::replace_file(&self.temp, &self.record).map_err(|_| "Record replacement failed; preserve it.")?;
        self.value = value;
        Ok(())
    }
}
