use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use ts_rs::{Config, TS};
use yokai_desktop_contract::{
    Availability, CommittedTurn, ConnectionState, ConversationView, FailureKind, IntentPhase,
    MAX_TEXT_BYTES, PendingIntent, SaveDraftRequest, SubmitRequest, ViewFailure, ViewSnapshot,
    WriteCertainty,
};

const DESTINATION: &str = "src/generated/desktop-contract.ts";

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 1 || args[0].as_os_str() != std::ffi::OsStr::new(DESTINATION) {
        return Err("Expected only src/generated/desktop-contract.ts from the admitted frontend root".into());
    }

    let output = std::env::current_dir()?.join(DESTINATION);
    check_parents(&output)?;
    let config = Config::default();
    let declarations = [
        ConnectionState::decl(&config),
        IntentPhase::decl(&config),
        FailureKind::decl(&config),
        WriteCertainty::decl(&config),
        ViewFailure::decl(&config),
        Availability::decl(&config),
        PendingIntent::decl(&config),
        CommittedTurn::decl(&config),
        ConversationView::decl(&config),
        ViewSnapshot::decl(&config),
        SubmitRequest::decl(&config),
        SaveDraftRequest::decl(&config),
    ];
    let mut source = format!(
        "// Generated from yokai-desktop-contract.\nexport const MAX_TEXT_BYTES = {MAX_TEXT_BYTES};\n\n"
    );
    for declaration in declarations {
        source.push_str("export ");
        source.push_str(&declaration);
        source.push_str("\n\n");
    }
    OpenOptions::new().write(true).create_new(true).open(output)?.write_all(source.as_bytes())?;
    Ok(())
}

fn check_parents(path: &Path) -> Result<(), Box<dyn Error>> {
    for parent in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(parent)?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = metadata.file_type().is_symlink();
        if !metadata.is_dir() || reparse {
            return Err("Generated output requires ordinary existing directories".into());
        }
    }
    Ok(())
}
