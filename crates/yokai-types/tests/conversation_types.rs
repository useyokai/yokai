use std::process::ExitCode;
use yokai_types::{CommandId, ConversationId, MessageId, SubmitMessage, MAX_USER_BYTES};

fn ids() {
    let bytes = [0x2a; 16];
    let conversation = ConversationId::from_bytes(bytes);
    let message = MessageId::from_bytes(bytes);
    let text = "2a2a2a2a-2a2a-2a2a-2a2a-2a2a2a2a2a2a";
    assert_eq!(conversation.to_string(), text);
    assert_eq!(message.to_string(), text);
    assert_eq!(text.parse(), Ok(conversation));
    assert_eq!(text.parse(), Ok(message));
    assert!(text.to_uppercase().parse::<MessageId>().is_err());
}

fn submission(text: &str) -> Result<SubmitMessage, yokai_types::TextLength> {
    SubmitMessage::new(
        CommandId::new("test-command").unwrap(),
        ConversationId::from_bytes([1; 16]),
        MessageId::from_bytes([2; 16]),
        text,
    )
}

fn bounds() {
    assert!(submission("").is_err());
    assert!(submission(&"a".repeat(MAX_USER_BYTES)).is_ok());
    assert!(submission(&"a".repeat(MAX_USER_BYTES + 1)).is_err());
    assert!(submission(&"🦊".repeat(MAX_USER_BYTES / 4)).is_ok());
    assert!(submission(&("🦊".repeat(MAX_USER_BYTES / 4) + "a")).is_err());
}

fn canonical() {
    let request = submission("é").unwrap();
    let mut expected = vec![1; 16];
    expected.extend([2; 16]);
    expected.extend([0, 0, 0, 2, 0xc3, 0xa9]);
    assert_eq!(request.canonical_payload(), expected);
    assert_eq!(request.canonical_payload(), submission("é").unwrap().canonical_payload());
    assert_ne!(request.canonical_payload(), submission("e\u{301}").unwrap().canonical_payload());
}

fn main() -> ExitCode {
    let protection = yokai_testsupport::make_non_dumpable();
    println!("{}", protection.line());
    if !protection.permits_checks() {
        return ExitCode::FAILURE;
    }
    for (id, check) in [("M1-ID", ids as fn()), ("M1-TEXT", bounds), ("M1-CANONICAL", canonical)] {
        if std::panic::catch_unwind(check).is_err() {
            println!("CHECK {id} FAIL");
            return ExitCode::FAILURE;
        }
        println!("CHECK {id} PASS");
    }
    ExitCode::SUCCESS
}
