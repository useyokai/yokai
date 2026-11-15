#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::time::{Duration, Instant};
    use yokai_ipc::{Endpoint, Enrollment, Remote, Reply, Request, REQUEST_MAX, RESPONSE_MAX, decode, encode, read_control};

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [endpoint_flag, endpoint, config_flag, config, operation] = args.as_slice() else {
        return Err("usage: native_client --endpoint <file> --peer-config <file> <status|create|submit|replay|shutdown>".into());
    };
    if endpoint_flag != "--endpoint" || config_flag != "--peer-config" {
        return Err("expected explicit endpoint and peer configuration".into());
    }
    let endpoint: Endpoint = read_control(Path::new(endpoint))?;
    let config: Enrollment = read_control(Path::new(config))?;
    let request = match operation.to_str() {
        Some("status") => Request::Status,
        Some("shutdown") => Request::Shutdown,
        Some("create" | "submit" | "replay") => {
            let mut bytes = Vec::new();
            io::stdin().take(REQUEST_MAX as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > REQUEST_MAX {
                return Err("request input exceeds frame limit".into());
            }
            let request = decode(&bytes)?;
            if !matches!((operation.to_str(), &request),
                (Some("create"), Request::Create { .. })
                | (Some("submit"), Request::Submit { .. })
                | (Some("replay"), Request::Replay { .. }))
            {
                return Err("request operation does not match the invocation".into());
            }
            request
        }
        _ => return Err("unsupported client operation".into()),
    };
    let mut client = Remote::connect(&endpoint, &config)?;
    let request_id = client.send(request)?;
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        let frame = client.receive(deadline)?;
        if frame.request != request_id {
            return Err("unexpected reply request ID".into());
        }
        let stdout = io::stdout();
        let mut output = stdout.lock();
        output.write_all(&encode(&frame.body, RESPONSE_MAX)?)?;
        output.write_all(b"\n")?;
        output.flush()?;
        match frame.body {
            Reply::Progress { .. } | Reply::ShutdownAccepted => {}
            Reply::Decision { code, .. } if code == "accepted" => {}
            _ => return Ok(()),
        }
    }
}

#[cfg(not(windows))]
fn main() -> Result<(), &'static str> {
    Err("local runtime transport requires Windows")
}
