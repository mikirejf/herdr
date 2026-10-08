//! Wall-clock time as unix milliseconds, for timestamps that survive restarts
//! and travel between machines. Use `Instant` for anything that only needs
//! elapsed time inside one process.

/// Milliseconds since the unix epoch, or 0 when the system clock is before it.
pub(crate) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
