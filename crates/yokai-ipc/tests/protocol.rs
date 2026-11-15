use yokai_ipc::{CONTROL_MAX, Event, REQUEST_MAX, RESPONSE_MAX, Reply, Request, decode, encode, length};

#[test]
fn gap_frame_matrix() {
    for n in [0, (REQUEST_MAX + 1) as u32, u32::MAX] { assert!(length(n.to_le_bytes(), REQUEST_MAX).is_err()); }
    for n in [1, REQUEST_MAX as u32] { assert_eq!(length(n.to_le_bytes(), REQUEST_MAX).unwrap(), n as usize); }
    for body in [b"".as_slice(), b"null", b"[]", b"{", b"\xff", br#"{"op":"status"}false"#,
        br#"{"op":"status","op":"status"}"#, br#"{"op":"status","authority":true}"#] {
        assert!(decode::<Request>(body).is_err());
    }
    let deep = format!("{}0{}", "[".repeat(256), "]".repeat(256));
    assert!(decode::<serde_json::Value>(deep.as_bytes()).is_err());
    let request = Request::Submit { command:"c".into(),conversation:"x".into(),message:"m".into(),text:"aβ𐐀\0".repeat(2048) };
    let bytes = encode(&request, REQUEST_MAX).unwrap();
    assert_eq!(encode(&request, bytes.len()).unwrap(), bytes);
    assert!(encode(&request, bytes.len()-1).is_err());
    assert!(decode::<Request>(&bytes).is_ok());
    let events = (1..=256).map(|sequence| Event {sequence,run:sequence,kind:"created".into(),message:None,text:None}).collect();
    let page = Reply::Replay { events, after:256,has_more:true };
    let wire = encode(&page, RESPONSE_MAX).unwrap();
    assert!(matches!(decode::<Reply>(&wire).unwrap(),Reply::Replay {events, ..} if events.len()==256));
    println!("CHECK G04 LENGTH JSON UTF8 ENCODED-BOUNDARY REPLAY-PAGE PASS");
}

#[test]
fn frame_length_is_checked_before_allocation() {
    for size in [0, (REQUEST_MAX + 1) as u32, u32::MAX] {
        assert!(length(size.to_le_bytes(), REQUEST_MAX).is_err());
    }
    assert_eq!(length((REQUEST_MAX as u32).to_le_bytes(), REQUEST_MAX).unwrap(), REQUEST_MAX);
}

#[test]
fn rejects_invalid_request_envelopes() {
    for request in [
        br#"{"op":"status","owner":"someone"}"#.as_slice(),
        br#"{"op":"status","op":"shutdown"}"#,
        br#"{"op":"status"} {}"#,
        br#"{"op":"submit","command":"c","conversation":"x","message":"y","text":"one","text":"two"}"#,
        br#"{"op":"execute","command":"something"}"#,
        b"{\"op\":\"status\",\"other\":\"\xff\"}",
    ] {
        assert!(decode::<Request>(request).is_err(), "{request:?}");
    }
}

#[test]
fn encoded_replay_accounts_for_control_character_expansion() {
    let events = (1..=3).map(|sequence| Event {
        sequence,
        run: sequence,
        kind: "completed".into(),
        message: Some("00000000-0000-0000-0000-000000000000".into()),
        text: Some("\0".repeat(65536)),
    }).collect();
    let reply = Reply::Replay { events, after: 3, has_more: false };
    let bytes = encode(&reply, RESPONSE_MAX).unwrap();
    assert!(bytes.len() > 1024 * 1024);
    assert!(matches!(decode::<Reply>(&bytes).unwrap(), Reply::Replay { after: 3, .. }));
    assert!(encode(&reply, CONTROL_MAX).is_err());
}


#[test]
fn empty_requests_keep_their_wire_encoding() {
    for (request, wire) in [
        (Request::Status, br#"{"op":"status"}"#.as_slice()),
        (Request::Shutdown, br#"{"op":"shutdown"}"#.as_slice()),
    ] {
        assert_eq!(encode(&request, REQUEST_MAX).unwrap(), wire);
        let decoded = decode::<Request>(wire).unwrap();
        assert!(matches!((request, decoded),
            (Request::Status, Request::Status) | (Request::Shutdown, Request::Shutdown)));
    }
}

#[test]
fn rejects_unknown_fields_on_shutdown() {
    for request in [
        br#"{"op":"shutdown","owner":"someone"}"#.as_slice(),
        br#"{"owner":null,"op":"shutdown"}"#,
        br#"{"op":"shutdown","extra":{"nested":[1,2,3]}}"#,
    ] {
        assert!(decode::<Request>(request).is_err(), "{request:?}");
    }
}

#[test]
fn empty_requests_are_strict_inside_frames() {
    use yokai_ipc::{Binding, Epoch, Frame, VERSION};

    for body in [Request::Status, Request::Shutdown] {
        let frame = Frame {
            binding: Binding {
                epoch: Epoch {
                    version: VERSION,
                    instance: "instance".into(),
                    runtime_epoch: 1,
                    nonce: "nonce".into(),
                },
                session: "session".into(),
            },
            sequence: 1,
            request: 1,
            body,
        };
        let wire = encode(&frame, REQUEST_MAX).unwrap();
        let decoded = decode::<Frame<Request>>(&wire).unwrap();
        assert!(matches!((&frame.body, &decoded.body),
            (Request::Status, Request::Status) | (Request::Shutdown, Request::Shutdown)));
        let mut value: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        value["body"]["owner"] = serde_json::json!("someone");
        let invalid = serde_json::to_vec(&value).unwrap();
        assert!(decode::<Frame<Request>>(&invalid).is_err());
    }
}
