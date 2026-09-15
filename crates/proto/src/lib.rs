//! holt-proto — wire types shared by engine, UI, and RPC.
//!
//! Ported from holt's `packages/control/src/wire.ts` + `packages/provider/src/types.ts`.
//! Token-usage types are part of the contract by design: `Model::context_window`
//! carries the context-window denominator and the usage ledger's records and
//! totals are served over the same RPC surface. The ledger itself never enters
//! a doc: the only usage-shaped values a doc part carries are display summaries
//! (a spawn chip's token total, a Compaction divider's estimate).

pub mod agent;
pub mod entities;
pub mod links;
pub mod motion;
pub mod view;
pub mod workspace;

pub use agent::*;
pub use entities::*;
pub use links::*;
pub use workspace::*;

/// Parse "0.2.12" (tolerating a `-suffix`/`+build` tail on the last part)
/// into a comparable triple — the fleet feature-gate primitive (device rows
/// stamp `Device::version` at boot). `None` for anything that doesn't lead
/// with three dotted integers, and gates treat `None` as "too old".
pub fn version_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().splitn(3, '.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch = parts.next()?;
    let patch: u64 = patch
        .split(['-', '+'])
        .next()
        .unwrap_or(patch)
        .parse()
        .ok()?;
    Some((major, minor, patch))
}
