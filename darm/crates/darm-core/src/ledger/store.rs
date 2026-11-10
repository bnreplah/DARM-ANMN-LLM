//! Append-only on-disk log of finalized blocks (one JSON block per line).
//!
//! The file is never trusted: [`load`] replays it through
//! [`Ledger::replay`], so a tampered or truncated log fails verification
//! instead of yielding a forged history. A torn final line (crash mid-write)
//! is reported, not silently skipped, so the operator can decide whether to
//! truncate it.

use super::locking::Vote;
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

/// Durable record of the votes this validator has signed for the height it
/// is currently deciding.
///
/// Lock-based BFT is only safe if a validator never signs two different
/// votes for one round and never forgets a lock. Both survive a restart only
/// if every vote reaches disk before it reaches the network: call
/// [`VoteLog::append`] first, and broadcast only if it succeeded.
#[derive(Debug, Clone)]
pub struct VoteLog {
    path: PathBuf,
}

/// Rounds are bounded by time, and each holds at most two votes of ours.
const MAX_VOTE_LOG_LINES: usize = 100_000;

impl VoteLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Write and fsync one vote.
    pub fn append(&self, vote: &Vote) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line =
            serde_json::to_vec(vote).map_err(|e| DarmError::malformed("vote", e.to_string()))?;
        line.push(b'\n');
        let mut file = open_append(&self.path)?;
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    /// The votes recorded for `height`, oldest first. A line that does not
    /// parse is an error, not a skip: silently dropping a vote could let
    /// this validator sign a conflicting one.
    pub fn load(&self, height: u64) -> Result<Vec<Vote>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut votes = Vec::new();
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if number >= MAX_VOTE_LOG_LINES || line.len() > 4096 {
                return Err(DarmError::LimitExceeded("vote log".into()));
            }
            let vote: Vote = serde_json::from_str(&line).map_err(|e| {
                DarmError::malformed("vote log", format!("line {}: {e}", number + 1))
            })?;
            if vote.height == height {
                votes.push(vote);
            }
        }
        Ok(votes)
    }

    /// Drop votes for heights that are final. Safe to lose: a finalized
    /// height can no longer be voted on.
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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
