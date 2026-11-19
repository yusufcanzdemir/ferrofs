//! Well-known ferrofs directories.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// `%APPDATA%\ferrofs\config.toml`
pub fn config_file() -> Result<PathBuf> {
    let base = std::env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(base).join("ferrofs").join("config.toml"))
}

/// `%LOCALAPPDATA%\ferrofs\logs`
pub fn log_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base).join("ferrofs").join("logs"))
}
