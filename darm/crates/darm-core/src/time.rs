use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since the Unix epoch, saturating at zero for clocks
/// set before 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
