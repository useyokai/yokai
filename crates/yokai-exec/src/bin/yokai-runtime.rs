#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::Path;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(feature = "test-api")]
    let args = if args.len() == 10 && args[7] == "--test-mode" {
        yokai_exec::local_runtime::test_api::prepare(args[8].to_str().ok_or("test mode")?, args[9].to_str().ok_or("test token")?)?;
        args[..7].to_vec()
    } else { args };
    let [operation, root_flag, root, control_flag, control, config_flag, config] = args.as_slice() else {
        return Err("usage: yokai-runtime create --parent <path> --control <dir> --peer-config <file>, or open --instance <path> with the same flags".into());
    };
    let root = Path::new(root);
    let control = Path::new(control);
    let config = Path::new(config);
    if control_flag != "--control" || config_flag != "--peer-config"
        || !root.is_absolute() || !control.is_absolute() || !config.is_absolute()
    {
        return Err("expected an explicit local runtime configuration with absolute paths".into());
    }
    let enrollment = yokai_ipc::read_control(config)?;
    let opened = if operation == "create" && root_flag == "--parent" {
        yokai_store::create_instance(root)
    } else if operation == "open" && root_flag == "--instance" {
        yokai_store::open_instance(root)
    } else {
        return Err("invalid runtime operation".into());
    };
    #[cfg(feature = "test-api")]
    eprintln!("STARTUP_CONNECTION_EVENTS {:?}", yokai_store::test_api::connection_events());
    let instance = opened?;
    yokai_exec::local_runtime::run(instance, control, &enrollment)
}

#[cfg(not(windows))]
fn main() -> Result<(), &'static str> {
    Err("local runtime transport requires Windows")
}
