//! Well-known ferrofs directories.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// `%LOCALAPPDATA%\ferrofs\logs`
pub fn log_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base).join("ferrofs").join("logs"))
}
