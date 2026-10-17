use std::fs;

use anyhow::{Context, Result};

use crate::envs::cache_dir;
use crate::ui::status;

pub fn run(quiet: bool) -> Result<()> {
    let dir = cache_dir()?;
    if !dir.exists() {
        status(quiet, format!("nothing to clean ({} does not exist)", dir.display()));
        return Ok(());
    }
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    status(quiet, format!("removed {}", dir.display()));
    Ok(())
}
