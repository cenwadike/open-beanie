use tokio::time::Duration;

/// How often each chain's reconciliation backstop re-scans everything
/// regardless of what its push subscription reported. Deliberately low
/// frequency on all three chains — it exists to catch a missed push
/// notification, not to do the main job. See each worker's own module doc
/// for what its reconciliation pass additionally does on top of the plain
/// re-scan (e.g. the balance-driven sweep backstop on the EVM and Solana
/// workers).
pub const RECONCILE_EVERY: Duration = Duration::from_secs(60);
