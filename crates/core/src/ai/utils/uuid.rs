//! Port of `pi-core/ai/src/utils/uuid.ts`: time-ordered UUIDv7 generation
//! with the TypeScript package's monotonic sequence semantics.
//!
//! The upstream implementation keeps a process-global `(lastTimestamp,
//! sequence)` pair: within one millisecond the counter increments, on a new
//! millisecond it re-seeds from fresh random bytes, and on counter overflow
//! it bumps the timestamp. That algorithm is reproduced here (the `uuid`
//! crate's own v7 monotonicity differs in the counter layout), with the clock
//! and randomness injectable for deterministic tests.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Generator state: `i64::MIN` plays the role of the TypeScript
/// `-Infinity` sentinel.
#[derive(Debug)]
pub struct UuidV7State {
    last_timestamp: i64,
    sequence: u32,
}

impl Default for UuidV7State {
    fn default() -> Self {
        Self {
            last_timestamp: i64::MIN,
            sequence: 0,
        }
    }
}

fn fill_random_bytes(bytes: &mut [u8; 16]) {
    getrandom::fill(bytes).expect("system RNG is always available");
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Formats 16 bytes as a hyphenated lowercase UUID.
fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Core generator with injected clock and randomness; mirrors the TypeScript
/// algorithm bit for bit.
pub fn uuidv7_with(state: &mut UuidV7State, timestamp: i64, random: &[u8; 16]) -> String {
    if timestamp > state.last_timestamp {
        state.sequence = (u32::from(random[6]) << 24)
            | (u32::from(random[7]) << 16)
            | (u32::from(random[8]) << 8)
            | u32::from(random[9]);
        state.last_timestamp = timestamp;
    } else {
        state.sequence = state.sequence.wrapping_add(1);
        if state.sequence == 0 {
            state.last_timestamp += 1;
        }
    }

    let ts = state.last_timestamp;
    let sequence = state.sequence;
    let mut bytes = [0u8; 16];
    bytes[0] = ((ts >> 40) & 0xff) as u8;
    bytes[1] = ((ts >> 32) & 0xff) as u8;
    bytes[2] = ((ts >> 24) & 0xff) as u8;
    bytes[3] = ((ts >> 16) & 0xff) as u8;
    bytes[4] = ((ts >> 8) & 0xff) as u8;
    bytes[5] = (ts & 0xff) as u8;
    bytes[6] = 0x70 | ((sequence >> 28) & 0x0f) as u8;
    bytes[7] = ((sequence >> 20) & 0xff) as u8;
    bytes[8] = 0x80 | ((sequence >> 14) & 0x3f) as u8;
    bytes[9] = ((sequence >> 6) & 0xff) as u8;
    bytes[10] = (((sequence & 0x3f) << 2) as u8) | (random[10] & 0x03);
    bytes[11] = random[11];
    bytes[12] = random[12];
    bytes[13] = random[13];
    bytes[14] = random[14];
    bytes[15] = random[15];
    format_uuid(&bytes)
}

static GLOBAL_STATE: Mutex<UuidV7State> = Mutex::new(UuidV7State {
    last_timestamp: i64::MIN,
    sequence: 0,
});

/// Port of the exported `uuidv7()`: a time-ordered UUIDv7.
pub fn uuidv7() -> String {
    let mut random = [0u8; 16];
    fill_random_bytes(&mut random);
    let timestamp = now_millis();
    let mut state = GLOBAL_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    uuidv7_with(&mut state, timestamp, &random)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMESTAMP: i64 = 0x0123456789ab;

    #[test]
    fn uses_rfc_9562_layout_and_preserves_monotonic_order() {
        let random_values: [[u8; 16]; 3] = [
            [
                0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xfe, 0x01, 0x11, 0x22, 0x33, 0x44, 0x55,
            ],
            [0; 16],
            [0; 16],
        ];

        let mut state = UuidV7State::default();
        let first = uuidv7_with(&mut state, TIMESTAMP, &random_values[0]);
        let second = uuidv7_with(&mut state, TIMESTAMP, &random_values[1]);
        let third = uuidv7_with(&mut state, TIMESTAMP, &random_values[2]);

        assert_eq!(first, "01234567-89ab-7fff-bfff-f91122334455");
        assert_eq!(second, "01234567-89ab-7fff-bfff-fc0000000000");
        assert_eq!(third, "01234567-89ac-7000-8000-000000000000");
        // The exact-value assertions above pin the RFC 9562 layout (version
        // nibble 7, variant bits 10).

        let parse_timestamp =
            |uuid: &str| i64::from_str_radix(&uuid.replace('-', "")[0..12], 16).unwrap();
        assert_eq!(parse_timestamp(&first), TIMESTAMP);
        assert_eq!(parse_timestamp(&second), TIMESTAMP);
        assert_eq!(parse_timestamp(&third), TIMESTAMP + 1);
        assert!(first < second);
        assert!(second < third);
    }
}
