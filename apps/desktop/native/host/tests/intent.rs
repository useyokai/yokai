use yokai_desktop::intent::Record;
use yokai_types::{CommandId, ConversationId, InstanceId, MessageId};

fn assert_canonical_intent(record: &Record) {
    let intent = record.intent.as_ref().unwrap();
    assert_eq!(record.instance.parse::<InstanceId>().unwrap().to_string(), record.instance);
    assert_eq!(record.conversation.as_ref(), Some(&intent.conversation));
    assert_eq!(intent.conversation.parse::<ConversationId>().unwrap().to_string(), intent.conversation);
    assert_eq!(intent.message.parse::<MessageId>().unwrap().to_string(), intent.message);
    for value in [&record.instance, &intent.conversation, &intent.message, &intent.submit_command]
        .into_iter().chain(intent.create_command.iter()) {
        yokai_desktop::intent::id(value).unwrap();
    }
    for command in std::iter::once(&intent.submit_command).chain(intent.create_command.iter()) {
        assert_eq!(CommandId::new(command).unwrap().as_str(), command.as_str());
    }
}

#[test]
fn record_is_instance_bound_strict_and_size_bounded() {
    let id = InstanceId::from_bytes([0x11; 16]).to_string();
    let record = Record::empty(id.clone());
    let bytes = record.bytes().unwrap();
    assert!(Record::decode(&bytes, &id).is_ok());
    let other = InstanceId::from_bytes([0x22; 16]).to_string();
    assert_eq!(Record::decode(&bytes, &other).err().as_deref(), Some("Invalid restoration record."));
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["unknown"] = true.into();
    assert!(Record::decode(&serde_json::to_vec(&value).unwrap(), &id).is_err());

    let canonical = InstanceId::from_bytes([0xab; 16]).to_string();
    for malformed in [
        "ab".repeat(16), canonical.to_uppercase(), canonical.replacen('-', "", 1),
        canonical.replacen('-', "_", 1), format!("{canonical}0"),
    ] {
        let mut invalid = record.clone();
        invalid.instance = malformed.clone();
        assert_eq!(invalid.bytes().err().as_deref(), Some("Invalid identity."));
        let mut invalid = record.clone();
        invalid.begin("hello".into()).unwrap();
        invalid.intent.as_mut().unwrap().message = malformed;
        assert_eq!(invalid.bytes().err().as_deref(), Some("Invalid identity."));
    }
    let mut record = record;
    record.draft = "\u{0001}".repeat(16384);
    assert!(record.bytes().is_err());
}

#[test]
fn restored_intent_keeps_ids_and_never_becomes_a_new_submit() {
    let mut record = Record::empty(InstanceId::from_bytes([0x11; 16]).to_string());
    record.begin("hello".into()).unwrap();
    assert_canonical_intent(&record);
    assert!(record.intent.as_ref().unwrap().create_command.is_some());
    let encoded = record.bytes().unwrap();
    let restored = Record::decode(&encoded, &record.instance).unwrap();
    assert_canonical_intent(&restored);
    assert_eq!(restored.instance, record.instance);
    assert_eq!(restored.conversation, record.conversation);
    let original = record.intent.as_ref().unwrap();
    let decoded = restored.intent.as_ref().unwrap();
    assert_eq!(decoded.create_command, original.create_command);
    assert_eq!(decoded.submit_command, original.submit_command);
    assert_eq!(decoded.conversation, original.conversation);
    assert_eq!(decoded.message, original.message);
    assert!(matches!(restored.intent.unwrap().phase,yokai_desktop::intent::Phase::Unknown));

    let mut existing = Record::empty(record.instance.clone());
    let conversation = ConversationId::from_bytes([0x33; 16]).to_string();
    existing.conversation = Some(conversation.clone());
    existing.begin("next message".into()).unwrap();
    assert_canonical_intent(&existing);
    assert_eq!(existing.conversation.as_ref(), Some(&conversation));
    assert!(existing.intent.as_ref().unwrap().create_command.is_none());
    let decoded = Record::decode(&existing.bytes().unwrap(), &existing.instance).unwrap();
    assert_canonical_intent(&decoded);
    let original = existing.intent.as_ref().unwrap();
    let decoded_intent = decoded.intent.as_ref().unwrap();
    assert_eq!(decoded.instance, existing.instance);
    assert_eq!(decoded.conversation, existing.conversation);
    assert_eq!(decoded_intent.create_command, original.create_command);
    assert_eq!(decoded_intent.submit_command, original.submit_command);
    assert_eq!(decoded_intent.conversation, original.conversation);
    assert_eq!(decoded_intent.message, original.message);
    assert!(matches!(decoded_intent.phase,yokai_desktop::intent::Phase::Unknown));
}

#[test]
fn create_must_be_acknowledged_before_dependent_submit() {
    use yokai_desktop::intent::Phase;
    let mut record=Record::empty(InstanceId::from_bytes([0x11; 16]).to_string()); record.draft="original".into(); record.begin("original".into()).unwrap();
    assert!(record.draft.is_empty());
    let intent=record.intent.as_mut().unwrap();
    assert!(!intent.ready_to_submit());
    intent.phase=Phase::Creating; assert!(!intent.ready_to_submit());
    intent.phase=Phase::Created; assert!(intent.ready_to_submit());
    intent.phase=Phase::Unknown; assert!(!intent.ready_to_submit());
    intent.phase=Phase::Accepted; intent.accepted_run=Some(3); assert!(!intent.ready_to_submit());
}

#[test]
fn contradictory_restoration_phase_and_run_are_refused() {
    use yokai_desktop::intent::Phase;
    let mut record=Record::empty(InstanceId::from_bytes([0x11; 16]).to_string()); record.begin("original".into()).unwrap();
    record.intent.as_mut().unwrap().phase=Phase::Accepted;
    assert!(record.bytes().is_err());
    record.intent.as_mut().unwrap().accepted_run=Some(3);
    assert!(record.bytes().is_ok());
    record.intent.as_mut().unwrap().phase=Phase::Prepared;
    assert!(record.bytes().is_err());
}

#[cfg(windows)]
fn fixture(name: &str) -> std::path::PathBuf {
    let root = std::path::PathBuf::from(std::env::var_os("YOKAI_HOST_TEST_ROOT").expect("a separately admitted fresh test root is required"));
    yokai_desktop::intent::ordinary(&root, true).unwrap();
    let path = root.join(name);
    std::fs::create_dir(&path).expect("test fixture must be unused");
    path
}

#[cfg(windows)]
#[test]
fn guard_refuses_second_owner_and_record_roundtrips_without_cleanup() {
    use yokai_desktop::intent::Store;
    let root = fixture("guard-and-record");
    let id = InstanceId::from_bytes([0x11; 16]).to_string();
    let mut owner = Store::open(&root, &id).unwrap();
    assert!(Store::open(&root, &id).is_err());
    let mut record = owner.value.clone();
    record.begin("original".into()).unwrap();
    let original = record.intent.as_ref().unwrap().submit_command.clone();
    owner.save(record).unwrap();
    assert!(root.join("intent.json").is_file());
    drop(owner);
    let restored = Store::open(&root, &id).unwrap();
    assert_eq!(restored.value.intent.as_ref().unwrap().submit_command, original);
    assert_eq!(std::fs::metadata(root.join("desktop.guard")).unwrap().len(), 0);
}

#[cfg(windows)]
#[test]
fn stale_temporary_file_is_preserved_and_blocks_replacement() {
    use yokai_desktop::intent::Store;
    let root = fixture("stale-temp");
    std::fs::write(root.join("intent.tmp"), b"preserved").unwrap();
    let id = InstanceId::from_bytes([0x11; 16]).to_string();
    assert_eq!(Store::open(&root, &id).err().as_deref(), Some("Retained temporary record; preserve it."));
    assert_eq!(std::fs::read(root.join("intent.tmp")).unwrap(), b"preserved");
}

#[cfg(windows)]
#[test]
fn failed_persistence_keeps_the_prior_record_and_temporary_evidence() {
    use yokai_desktop::intent::Store;
    let root=fixture("failed-persistence"); let id=InstanceId::from_bytes([0x11; 16]).to_string();
    let mut store=Store::open(&root,&id).unwrap();
    let old=store.value.clone(); store.save(old.clone()).unwrap();
    let original=std::fs::read(root.join("intent.json")).unwrap();
    std::fs::write(root.join("intent.tmp"),b"blocked").unwrap();
    let mut next=old; next.begin("never dispatched".into()).unwrap();
    assert!(store.save(next).is_err());
    assert!(store.value.intent.is_none());
    assert_eq!(std::fs::read(root.join("intent.json")).unwrap(),original);
    assert_eq!(std::fs::read(root.join("intent.tmp")).unwrap(),b"blocked");
}

#[cfg(windows)]
#[test]
fn ownership_only_host_holds_guard_after_worker_joins() {
    use yokai_desktop::connection::Bridge;
    let root=fixture("ownership-only"); let owner=Bridge::start(None,&root).unwrap();
    assert!(Bridge::start(None,&root).is_err());
    owner.request_close();
    let end=std::time::Instant::now()+std::time::Duration::from_secs(2);
    while !owner.join_finished().unwrap() {
        assert!(std::time::Instant::now()<end,"test worker did not close");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(Bridge::start(None,&root).is_err());
    // Fixture/guard are deliberately retained; host ownership ends with owner drop.
}
