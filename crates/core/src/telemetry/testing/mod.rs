//! Port of `pi-core/telemetry/src/testing/index.ts`: the runner-independent
//! conformance suite for callback telemetry adapters.

mod conformance;
mod types;

pub use conformance::create_telemetry_adapter_conformance;
pub use types::{
    TelemetryAdapterConformanceCase, TelemetryAdapterFixture, TelemetryAdapterFixtureFactory,
};
