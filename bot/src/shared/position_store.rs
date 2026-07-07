//! Position persistence .
//!
//! Writes rate-arb positions to disk as JSON on every register/remove so that
//! a crash or upgrade never loses track of open leveraged positions.
//! On startup, positions are reloaded and re-registered into HealthFactorMonitor.

use anyhow::Result;
use serde::{Serialize, Deserialize};
use std::path::Path;

/// Save a serializable slice to a JSON file atomically (write-then-rename).
pub fn save<T: Serialize>(path: &str, items: &[T]) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = format!("{}.tmp", path);
    let json = serde_json::to_string_pretty(items)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;   // atomic on POSIX
    Ok(())
}

/// Load positions from JSON file. Returns empty vec if file doesn't exist.
pub fn load<T: for<'de> Deserialize<'de>>(path: &str) -> Result<Vec<T>> {
    if !Path::new(path).exists() {
        return Ok(vec![]);
    }
    let data = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}
