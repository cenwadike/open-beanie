//! Chunk-level log cache and checkpoint store, backed by sled.
//!
//! WHY THIS MODULE EXISTS AND WHY IT OWNS CHECKPOINTS
//! -------------------------------------------------------
//! The actual bug that was compounding the Infura rate-limit errors wasn't
//! "not enough backoff" — it was that `discover_registry_activity` and
//! `fetch_deposits_since_block` only committed progress (the watermark) if
//! the *entire* requested block range succeeded. A backlog of tens of
//! thousands of chunks, one failure anywhere in it, and every chunk
//! already fetched — already paid for in provider credits — was thrown
//! away. The next attempt re-requested the same range from the same
//! starting point, guaranteeing another failure, often sooner.
//!
//! Fixing that properly means progress has to be durable *per chunk*, not
//! per whole-range call. That's what this module is: every chunk's
//! decoded result is written here the moment it's fetched, and the
//! checkpoint advances in the same breath. If the 401st chunk of 30,000
//! fails, chunks 1-400 are never re-fetched, ever again — not in this
//! process, not after a restart, not from a different data source.
//!
//! WHY THIS IS THE *SINGLE* SOURCE OF TRUTH, NOT A CACHE IN FRONT OF ONE
//! -------------------------------------------------------------------------
//! It would be tempting to keep the in-memory watermark fields on
//! `EvmState`/`StarknetState` and just add a cache lookup before each RPC
//! call. That's a patch, not a fix: it leaves two places that can
//! disagree about how far the scan has gotten, and it does nothing for a
//! process restart (the in-memory field resets, the cache doesn't, so
//! which one wins?). Instead, the checkpoint lives here and *only* here.
//! Callers ask this module "where do I resume from," never a struct field.
//!
//! WHY THIS MODULE DOESN'T KNOW ABOUT ETHERS, ALLOY, OR STARKNET TYPES
//! ------------------------------------------------------------------------
//! Deliberately generic: `put_chunk`/`get_chunk` take any
//! `Serialize`/`DeserializeOwned` type. Callers (evm_keeper.rs,
//! starknet_keeper.rs, and the explorer backfill modules) define their own
//! small serializable row types using plain hex strings for
//! addresses/felts rather than passing `ethers::types::Address` or
//! `starknet::core::types::Felt` in here directly. That keeps this module
//! chain-agnostic and reusable by both a live-RPC-sourced chunk and an
//! explorer-API-sourced chunk interchangeably — the cache has no idea
//! (and no reason to care) which source filled a given chunk in.
//!
//! SLED NOTE
//! -----------
//! sled's 0.34 line is what you actually depend on today; its release
//! cadence has been slow for a long time. That's a fact worth knowing, not
//! a reason to swap it out here — this module only uses sled's stable,
//! long-standing core API (open, a couple of named trees, get/insert),
//! nothing from an unreleased/edge feature set.
//!
//! Add to Cargo.toml:
//!   sled = "0.34"
//!   serde = { version = "1", features = ["derive"] }
//!   serde_json = "1"

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
