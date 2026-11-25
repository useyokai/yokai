fn main() -> Result<(), String> {
    let code = yokai_desktop::run()?;
    if code == 0 { Ok(()) } else { Err(format!("Native host returned {code}; preserve the case evidence.")) }
}
