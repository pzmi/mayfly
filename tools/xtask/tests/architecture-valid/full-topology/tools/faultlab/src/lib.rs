//! Positive fixture: the fault lab drives the binaries as processes and imports no
//! workspace crate (an independent oracle).

pub fn kill(pid: u32) -> std::io::Result<bool> {
    let status = std::process::Command::new("kill").arg(pid.to_string()).status()?;
    Ok(status.success())
}
