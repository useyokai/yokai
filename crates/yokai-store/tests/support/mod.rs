//! Filesystem assertions shared by the runtime test harnesses.
use std::path::Path;

pub fn entry_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("metadata {}: {error}", path.display())),
    }
}

pub fn verify_absence_oracle(parent: &Path) -> Result<(), String> {
    if !entry_exists(parent)? || entry_exists(&parent.join("absence-oracle-missing"))? {
        return Err("absence oracle confused an existing directory and a fresh missing entry".into());
    }
    if entry_exists(&parent.join("invalid\0entry")).is_ok() {
        return Err("absence oracle reported invalid-path metadata failure as absence".into());
    }
    println!("INFO strict_absence existing_directory=present missing=NotFound invalid_path=error");
    Ok(())
}
