//! Append-only on-disk log of finalized blocks (one JSON block per line).
//!
//! The file is never trusted: [`load`] replays it through
//! [`Ledger::replay`], so a tampered or truncated log fails verification
//! instead of yielding a forged history. A torn final line (crash mid-write)
//! is reported, not silently skipped, so the operator can decide whether to
//! truncate it.

use super::{Block, ChainConfig, Ledger};
use crate::{DarmError, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Largest single block line accepted when loading (defensive bound).
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct BlockLog {
    path: PathBuf,
}

impl BlockLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self, config: ChainConfig) -> Result<Ledger> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Ledger::new(config));
            }
            Err(error) => return Err(error.into()),
        };
        let mut blocks = Vec::new();
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                return Err(DarmError::LimitExceeded(format!(
                    "block log line {} is too large",
                    number + 1
                )));
            }
            let block: Block = serde_json::from_str(&line).map_err(|e| {
                DarmError::malformed("block log", format!("line {}: {e}", number + 1))
            })?;
            blocks.push(block);
        }
        Ledger::replay(config, blocks)
    }

    pub fn append(&self, block: &Block) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line =
            serde_json::to_vec(block).map_err(|e| DarmError::malformed("block", e.to_string()))?;
        line.push(b'\n');
        let mut file = open_append(&self.path)?;
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }
}

#[cfg(unix)]
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}
