#![cfg(windows)]

use sha2::{Digest, Sha256};
use std::io;
use std::time::{Duration, Instant};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{PIPE_REJECT_REMOTE_CLIENTS, WaitNamedPipeW};
use yokai_platform::local_ipc::{Client, ImagePin, IoError, Listener, PipeName, adapter_flags, current_identity};
use yokai_platform::random_instance_id;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn name() -> PipeName {
    PipeName::from_nonce(&random_instance_id().unwrap().to_hex()).unwrap()
}

fn image() -> ImagePin {
    let path = std::env::current_exe().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let digest: String = Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    ImagePin::open(&path, &digest).unwrap()
}

#[test]
fn local_only_flags_and_names() {
    let (open, mode) = adapter_flags();
    assert_eq!(open, FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION);
    assert_eq!(mode & PIPE_REJECT_REMOTE_CLIENTS, PIPE_REJECT_REMOTE_CLIENTS);
    for name in ["", r"\\server\pipe\other", "0123456789abcdef0123456789abcde/"] {
        assert!(PipeName::from_nonce(name).is_err());
    }
}

#[test]
fn connects_with_enrollment_and_refuses_second_pipe_owner() {
    let name = name();
    let listener = Listener::new(&name).unwrap();
    assert!(Listener::new(&name).is_err());
    let image = image();
    let client = Client::connect(&name, &image, current_identity().unwrap()).unwrap();
    listener.accept(deadline()).unwrap();
    listener.authenticate(&image).unwrap();
    client.pipe.write_all(b"hello", deadline()).unwrap();
    let mut bytes = [0; 5];
    listener.pipe().read_exact(&mut bytes, deadline()).unwrap();
    assert_eq!(&bytes, b"hello");
    listener.disconnect().unwrap();
}

#[test]
fn stale_creation_time_and_pending_read_deadline() {
    let name = name();
    let listener = Listener::new(&name).unwrap();
    let image = image();
    let identity = current_identity().unwrap();
    let stale = yokai_platform::local_ipc::ProcessIdentity { creation_time: identity.creation_time - 1, ..identity };
    let Err(error) = Client::connect(&name, &image, stale) else {
        panic!("stale pipe identity was accepted");
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "pipe peer identity mismatch");
    listener.disconnect().unwrap();

    let pipe_name: Vec<u16> = name.as_str().encode_utf16().chain([0]).collect();
    let reconnect_deadline = deadline();
    let client = std::thread::scope(|scope| {
        let accept = scope.spawn(|| listener.accept(reconnect_deadline));
        let remaining = reconnect_deadline.checked_duration_since(Instant::now())
            .expect("reconnect deadline elapsed before waiting");
        let wait_ms = u32::try_from(remaining.as_millis().max(1)).unwrap();
        // A disconnected instance becomes available once the next accept is pending.
        if unsafe { WaitNamedPipeW(pipe_name.as_ptr(), wait_ms) } == 0 {
            panic!("waiting for the listener to accept again: {}", io::Error::last_os_error());
        }
        let client = Client::connect(&name, &image, identity).expect("reconnect to rearmed listener");
        accept.join().unwrap().expect("accept reconnected client");
        client
    });
    let mut output = [0; 1];
    assert!(matches!(
        listener.pipe().read_exact(&mut output, Instant::now() + Duration::from_millis(30)),
        Err(IoError::Deadline)
    ));
    assert!(!listener.pipe().retained());
    client.pipe.write_all(b"x", deadline()).unwrap();
    listener.pipe().read_exact(&mut output, deadline()).unwrap();
    assert_eq!(&output, b"x");
    listener.disconnect().unwrap();
}


#[test]
fn concurrent_server_reader_and_writer_make_bounded_progress() {
    let name = name();
    let listener = Listener::new(&name).unwrap();
    let image = image();
    let client = Client::connect(&name, &image, current_identity().unwrap()).unwrap();
    listener.accept(deadline()).unwrap();
    listener.authenticate(&image).unwrap();
    let request = vec![0x6d; 4853];
    let response = vec![0x93; 16384];
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| listener.pipe().write_all(&response, deadline()));
        let reader = scope.spawn(|| {
            let mut actual = vec![0; request.len()];
            listener.pipe().read_exact(&mut actual, deadline())?;
            assert_eq!(actual, request);
            Ok::<_, IoError>(())
        });
        client.pipe.write_all(&request, deadline()).unwrap();
        let mut actual = vec![0; response.len()];
        client.pipe.read_exact(&mut actual, deadline()).unwrap();
        assert_eq!(actual, response);
        reader.join().unwrap().unwrap();
        writer.join().unwrap().unwrap();
    });
    assert!(!listener.pipe().retained());
    listener.disconnect().unwrap();
}

#[test]
fn read_deadline_does_not_cancel_the_simultaneous_write() {
    let name = name();
    let listener = Listener::new(&name).unwrap();
    let image = image();
    let client = Client::connect(&name, &image, current_identity().unwrap()).unwrap();
    listener.accept(deadline()).unwrap();
    listener.authenticate(&image).unwrap();
    let response = vec![0x42; 16384];
    std::thread::scope(|scope| {
        let (started, observed) = std::sync::mpsc::sync_channel(1);
        let pipe = listener.pipe();
        let bytes = &response;
        let writer = scope.spawn(move || {
            started.send(()).unwrap();
            pipe.write_all(bytes, deadline())
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut byte = [0];
        assert!(matches!(listener.pipe().read_exact(
            &mut byte, Instant::now() + Duration::from_millis(30)), Err(IoError::Deadline)));
        assert!(!listener.pipe().retained());
        assert!(!writer.is_finished());
        let mut actual = vec![0; response.len()];
        client.pipe.read_exact(&mut actual, deadline()).unwrap();
        assert_eq!(actual, response);
        writer.join().unwrap().unwrap();
    });
    // Read reuse requires the canceled read to have completed independently of the write.
    client.pipe.write_all(b"x", deadline()).unwrap();
    let mut actual = [0];
    listener.pipe().read_exact(&mut actual, deadline()).unwrap();
    assert_eq!(&actual, b"x");
    listener.disconnect().unwrap();
}

#[test]
fn gap_auth_boundaries() {
    use yokai_platform::local_ipc::{take_open_calls, fail_next_peer_query};
    take_open_calls();
    let pipe_name = name();
    let listener = Listener::new(&pipe_name).unwrap();
    assert!(listener.foreign_sid_denied().unwrap());
    let image = image();
    let identity = current_identity().unwrap();
    for fault in [1, 2] {
        let client = std::thread::scope(|scope| {
            let accepted = scope.spawn(|| listener.accept(deadline()));
            let wide: Vec<u16> = pipe_name.as_str().encode_utf16().chain([0]).collect();
            assert_ne!(unsafe { WaitNamedPipeW(wide.as_ptr(), 2000) }, 0, "rearmed listener");
            let client = Client::connect(&pipe_name, &image, identity).unwrap();
            accepted.join().unwrap().unwrap();
            client
        });
        fail_next_peer_query(fault);
        assert!(listener.authenticate(&image).is_err());
        // No authenticated Peer was returned, so no service can be invoked.
        listener.disconnect().unwrap();
        drop(client);
    }
    let calls = take_open_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|flags| *flags == FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION));
    println!("CHECK G01 NATIVE-ACL SQOS-INITIAL-RECONNECT PEER-QUERY-REFUSAL PASS");
}
