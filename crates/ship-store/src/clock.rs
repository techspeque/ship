//! Clock and entropy for new ids and timestamps. `ship-core` reads neither,
//! so the store supplies them.

use std::time::{SystemTime, UNIX_EPOCH};

use ship_core::Timestamp;
use ship_core::id::MAX_TIMESTAMP_MS;

use crate::error::StoreError;

/// Milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> Result<u64, StoreError> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::Clock)?
        .as_millis();
    u64::try_from(ms)
        .ok()
        .filter(|&ms| ms <= MAX_TIMESTAMP_MS)
        .ok_or(StoreError::Clock)
}

/// Now, as a [`Timestamp`].
pub(crate) fn now() -> Result<Timestamp, StoreError> {
    let ms = i64::try_from(now_millis()?).map_err(|_| StoreError::Clock)?;
    Timestamp::from_unix_millis(ms).map_err(|_| StoreError::Clock)
}

/// A new id stamped with `at`, from one of the `ship-core` `from_parts`
/// constructors.
pub(crate) fn new_id<T>(at: Timestamp, make: fn(u64, u128) -> Option<T>) -> Result<T, StoreError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| StoreError::Entropy)?;
    let ms = u64::try_from(at.unix_millis()).map_err(|_| StoreError::Clock)?;
    make(ms, u128::from_le_bytes(bytes)).ok_or(StoreError::Clock)
}
