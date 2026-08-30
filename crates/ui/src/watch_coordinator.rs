//! Shared policy for engine watch tasks.
//!
//! The gpui task owns the final state update, but subscription policy and frame
//! decoding live behind this small seam so every watch has the same behavior.

use std::time::Duration;

use serde::de::DeserializeOwned;

pub(crate) struct WatchCoordinator;

impl WatchCoordinator {
    pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);

    pub(crate) fn decode<T: DeserializeOwned>(
        value: serde_json::Value,
    ) -> Result<T, serde_json::Error> {
        serde_json::from_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::WatchCoordinator;

    #[test]
    fn malformed_frames_fail_at_the_coordinator_seam() {
        let result = WatchCoordinator::decode::<Vec<String>>(serde_json::json!({"bad": true}));
        assert!(result.is_err());
    }
}
