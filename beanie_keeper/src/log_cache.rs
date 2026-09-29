//! Chunk-level log cache and checkpoint store, backed by sled.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

/// One sled database, two trees: chunk results and checkpoints. Kept as
/// separate trees (not just prefixed keys in one tree) so a checkpoint
/// lookup never has to scan past chunk data, and so the two can be
/// reasoned about independently if this ever needs a "clear the cache but
/// keep checkpoints" or vice versa operation.
pub struct LogCache {
    chunks: sled::Tree,
    checkpoints: sled::Tree,
}

impl LogCache {
    /// Opens (or creates) the on-disk cache at `path`. One `LogCache`
    /// should be opened once at process startup and shared (via `Arc`)
    /// across the EVM and Starknet scan paths — sled itself is already
    /// internally thread-safe, so a single shared instance is the correct
    /// pattern, not a foundation-per-caller workaround.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = sled::open(path).context("failed opening sled log cache")?;
        let chunks = db
            .open_tree("chunks")
            .context("failed opening sled 'chunks' tree")?;
        let checkpoints = db
            .open_tree("checkpoints")
            .context("failed opening sled 'checkpoints' tree")?;
        Ok(Self {
            chunks,
            checkpoints,
        })
    }

    /// Key for one chunk's cached result. Zero-padded block numbers so
    /// keys sort lexicographically in the same order as numerically —
    /// mainly a debugging convenience (`sled` CLI tools / iteration order
    /// end up human-readable), not required for correctness since lookups
    /// here are always direct `get`s, never range scans.
    fn chunk_key(scan_id: &str, chunk_start: u64, chunk_end: u64) -> String {
        format!("{scan_id}:{chunk_start:020}:{chunk_end:020}")
    }

    /// Returns the previously-cached result for this exact chunk
    /// (`scan_id`, `chunk_start`, `chunk_end` must match exactly what was
    /// passed to `put_chunk` — see `chunk_ranges` below for why every
    /// caller should derive chunk boundaries the same way rather than
    /// picking their own).
    pub fn get_chunk<T: DeserializeOwned>(
        &self,
        scan_id: &str,
        chunk_start: u64,
        chunk_end: u64,
    ) -> Result<Option<T>> {
        let key = Self::chunk_key(scan_id, chunk_start, chunk_end);
        match self
            .chunks
            .get(key.as_bytes())
            .context("sled get failed for chunk")?
        {
            Some(bytes) => {
                let value = serde_json::from_slice(&bytes)
                    .context("failed deserializing cached chunk — cache format may have changed")?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Persists a chunk's result. Callers should call this immediately
    /// after a chunk is successfully fetched and decoded — before moving
    /// on to the next chunk, and before doing anything else that could
    /// fail — so a later failure can never un-happen an already-committed
    /// chunk.
    pub fn put_chunk<T: Serialize>(
        &self,
        scan_id: &str,
        chunk_start: u64,
        chunk_end: u64,
        value: &T,
    ) -> Result<()> {
        let key = Self::chunk_key(scan_id, chunk_start, chunk_end);
        let bytes = serde_json::to_vec(value).context("failed serializing chunk for cache")?;
        self.chunks
            .insert(key.as_bytes(), bytes)
            .context("sled insert failed for chunk")?;
        Ok(())
    }

    /// The last block this scan has fully, durably processed through.
    /// `None` means this scan has never made progress — the caller should
    /// fall back to its configured start block.
    pub fn get_checkpoint(&self, scan_id: &str) -> Result<Option<u64>> {
        match self
            .checkpoints
            .get(scan_id.as_bytes())
            .context("sled get failed for checkpoint")?
        {
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_ref()
                    .try_into()
                    .context("checkpoint value is not 8 bytes — cache is corrupt")?;
                Ok(Some(u64::from_be_bytes(arr)))
            }
            None => Ok(None),
        }
    }

    /// Advances the checkpoint. Callers should only ever move this
    /// forward (this method doesn't enforce that itself — it's a thin,
    /// honest wrapper — but every caller in this codebase computes the new
    /// checkpoint as `chunk_end` of a chunk it just finished, which is
    /// monotonic by construction of the chunking loop).
    pub fn set_checkpoint(&self, scan_id: &str, block: u64) -> Result<()> {
        self.checkpoints
            .insert(scan_id.as_bytes(), &block.to_be_bytes())
            .context("sled insert failed for checkpoint")?;
        Ok(())
    }

    /// Blocks until pending writes are durable on disk. Cheap to call
    /// after each chunk in practice (sled batches internally), but callers
    /// doing a large backfill burst may prefer to flush every N chunks
    /// instead of every single one — left to the caller's judgment rather
    /// than forced here.
    pub fn flush(&self) -> Result<()> {
        self.chunks.flush().context("sled flush failed (chunks)")?;
        self.checkpoints
            .flush()
            .context("sled flush failed (checkpoints)")?;
        Ok(())
    }
}

/// Splits `from..=to` into fixed-size `(start, end)` chunks of at most
/// `step` blocks each, inclusive on both ends.
///
/// WHY THIS IS SHARED, NOT REIMPLEMENTED PER CALLER
/// ----------------------------------------------------
/// `LogCache::get_chunk`/`put_chunk` key on the *exact* `(chunk_start,
/// chunk_end)` pair. If the live RPC scan and the explorer backfill ever
/// computed chunk boundaries slightly differently (off-by-one on an
/// inclusive/exclusive end, a different `step` default), they would write
/// and read completely different cache keys and silently never share a
/// single cached chunk between them — defeating the entire point of a
/// shared cache. Every scan path in this codebase — RPC-based or
/// explorer-based, EVM or Starknet — must compute chunks by calling this
/// function, never by hand-rolling the same loop shape again.
pub fn chunk_ranges(from: u64, to: u64, step: u64) -> impl Iterator<Item = (u64, u64)> {
    let step = step.max(1);
    let mut cursor = from;
    std::iter::from_fn(move || {
        if cursor > to {
            return None;
        }
        let end = (cursor + step - 1).min(to);
        let range = (cursor, end);
        cursor = end + 1;
        Some(range)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Row(u64);

    #[test]
    fn chunk_ranges_covers_inclusive_range_without_overlap() {
        let ranges: Vec<_> = chunk_ranges(10, 25, 5).collect();
        assert_eq!(ranges, vec![(10, 14), (15, 19), (20, 24), (25, 25)]);
    }

    #[test]
    fn round_trips_chunk_and_checkpoint() {
        let dir = tempfile_dir();
        let cache = LogCache::open(&dir).unwrap();

        assert_eq!(cache.get_checkpoint("test-scan").unwrap(), None);
        assert_eq!(cache.get_chunk::<Row>("test-scan", 0, 99).unwrap(), None);

        cache.put_chunk("test-scan", 0, 99, &Row(42)).unwrap();
        cache.set_checkpoint("test-scan", 99).unwrap();

        assert_eq!(
            cache.get_chunk::<Row>("test-scan", 0, 99).unwrap(),
            Some(Row(42))
        );
        assert_eq!(cache.get_checkpoint("test-scan").unwrap(), Some(99));

        std::fs::remove_dir_all(dir).ok();
    }

    fn tempfile_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("log_cache_test_{}", std::process::id()))
    }
}
