//! Rust port of the `@earendil-works/pi-*` v0.84.4 TypeScript packages:
//! [`pi-telemetry`], [`pi-ai`], and [`pi-agent-core`].
//!
//! Holt-owned since it merged into the Holt workspace (ADR-0043): the
//! upstream TypeScript sources, which were the port's behavioral
//! specification and test oracle, are no longer tracked. This crate
//! reproduces their observable behavior while exposing an idiomatic Rust
//! API; it does not require Node.js at build time or runtime.
//!
//! [`pi-telemetry`]: telemetry/index.html
//! [`pi-ai`]: ai/index.html
//! [`pi-agent-core`]: agent/index.html

pub mod agent;
pub mod ai;
pub mod telemetry;
