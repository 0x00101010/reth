//! Per-block commit positions, kept next to the database so pruning can honor a retention window.
//!
//! QMDB can reopen (and so rewind) at an older commit only if the log still holds everything
//! from that commit's sync boundary onward, so the prune point for "keep the last N blocks" is
//! the sync boundary recorded at the newest block at least N blocks old.

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

/// Log position of one persisted block's commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    /// Block number.
    pub(crate) number: u64,
    /// Operations in the log after the block's commit.
    pub(crate) size: u64,
    /// Safe prune point that still lets the database reopen at this commit.
    pub(crate) sync_boundary: u64,
}

const RECORD_LEN: usize = 24;

/// Rewrite the file once this many records have been dropped from its front.
const REWRITE_AFTER_DROPS: usize = 1 << 20;

/// Append-only file of [`Checkpoint`]s, oldest first, mirrored in memory.
#[derive(Debug)]
pub(crate) struct Checkpoints {
    path: PathBuf,
    file: File,
    entries: VecDeque<Checkpoint>,
    dropped: usize,
}

impl Checkpoints {
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("checkpoints.bin");
        let mut bytes = Vec::new();
        if let Ok(mut file) = File::open(&path) {
            file.read_to_end(&mut bytes)?;
        }
        // A torn trailing record from a crash mid-append is ignored.
        let entries = bytes.chunks_exact(RECORD_LEN).map(decode).collect();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, file, entries, dropped: 0 })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Appends `checkpoints` without fsync, so commits don't wait on it. A crash can lose the
    /// newest records; the prune target then comes from older ones, which only prunes less.
    pub(crate) fn append(&mut self, checkpoints: &[Checkpoint]) -> io::Result<()> {
        let bytes: Vec<u8> = checkpoints.iter().flat_map(encode).collect();
        self.file.write_all(&bytes)?;
        self.entries.extend(checkpoints);
        Ok(())
    }

    /// The checkpoint to prune to when keeping `retain` blocks below `tip`: the newest one at
    /// or below `tip - retain`, or the oldest one when none is that old.
    pub(crate) fn prune_target(&self, tip: u64, retain: u64) -> Option<Checkpoint> {
        let target = tip.saturating_sub(retain);
        self.entries
            .iter()
            .rev()
            .find(|c| c.number <= target)
            .or_else(|| self.entries.front())
            .copied()
    }

    /// Forgets checkpoints older than `number`; they can no longer be reopened at.
    pub(crate) fn drop_before(&mut self, number: u64) -> io::Result<()> {
        while self.entries.front().is_some_and(|c| c.number < number) {
            self.entries.pop_front();
            self.dropped += 1;
        }
        if self.dropped >= REWRITE_AFTER_DROPS {
            let tmp = self.path.with_extension("tmp");
            let bytes: Vec<u8> = self.entries.iter().flat_map(encode).collect();
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            self.file = OpenOptions::new().append(true).open(&self.path)?;
            self.dropped = 0;
        }
        Ok(())
    }
}

fn encode(c: &Checkpoint) -> [u8; RECORD_LEN] {
    let mut out = [0u8; RECORD_LEN];
    out[..8].copy_from_slice(&c.number.to_be_bytes());
    out[8..16].copy_from_slice(&c.size.to_be_bytes());
    out[16..].copy_from_slice(&c.sync_boundary.to_be_bytes());
    out
}

fn decode(bytes: &[u8]) -> Checkpoint {
    let word = |i: usize| u64::from_be_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
    Checkpoint { number: word(0), size: word(8), sync_boundary: word(16) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(number: u64) -> Checkpoint {
        Checkpoint { number, size: number * 10, sync_boundary: number * 5 }
    }

    #[test]
    fn targets_newest_old_enough_checkpoint_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut checkpoints = Checkpoints::open(dir.path()).unwrap();
        assert_eq!(checkpoints.prune_target(100, 10), None);
        checkpoints.append(&[cp(50), cp(60), cp(70)]).unwrap();
        // Nothing is 10 blocks older than 55: fall back to the oldest checkpoint.
        assert_eq!(checkpoints.prune_target(55, 10), Some(cp(50)));
        assert_eq!(checkpoints.prune_target(75, 10), Some(cp(60)));
        assert_eq!(checkpoints.prune_target(1000, 10), Some(cp(70)));

        checkpoints.drop_before(60).unwrap();
        assert_eq!(checkpoints.prune_target(55, 10), Some(cp(60)));
        drop(checkpoints);
        // Dropping is in memory until a rewrite; the file still has all records.
        let reopened = Checkpoints::open(dir.path()).unwrap();
        assert_eq!(reopened.prune_target(75, 10), Some(cp(60)));
        assert_eq!(reopened.entries.len(), 3);
    }
}
