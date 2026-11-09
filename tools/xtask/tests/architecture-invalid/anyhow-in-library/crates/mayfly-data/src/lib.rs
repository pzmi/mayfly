//! Fixture: a library returns `anyhow::Result`; callers can no longer match on its errors.

pub fn execute() -> anyhow::Result<()> {
    Ok(())
}
