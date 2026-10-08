//! Port of `pi-core/telemetry/src/testing/types.ts`.

use std::future::Future;
use std::sync::Arc;

use crate::telemetry::{BoxSpanFuture, RecordedTelemetrySpan, TelemetryContext};

/// Port of `TelemetryAdapterFixture`: a fresh adapter instance and normalized
/// snapshot reader owned by one conformance case. TypeScript disposal happens
/// through `Symbol.asyncDispose`; the Rust fixture is dropped when the case
/// completes.
pub trait TelemetryAdapterFixture: Send {
    fn context(&self) -> Arc<dyn TelemetryContext>;

    fn get_spans(&self) -> BoxSpanFuture<'_, Vec<RecordedTelemetrySpan>>;
}

/// Port of `TelemetryAdapterFixtureFactory`.
pub type TelemetryAdapterFixtureFactory =
    Arc<dyn Fn() -> BoxSpanFuture<'static, Box<dyn TelemetryAdapterFixture>> + Send + Sync>;

/// Port of `TelemetryAdapterConformanceCase`: a runner-independent conformance
/// case that can be registered with any test framework.
pub struct TelemetryAdapterConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    run: Arc<dyn Fn() -> BoxSpanFuture<'static, ()> + Send + Sync>,
}

impl TelemetryAdapterConformanceCase {
    pub(crate) fn new(
        group: &'static str,
        name: &'static str,
        run: Arc<dyn Fn() -> BoxSpanFuture<'static, ()> + Send + Sync>,
    ) -> Self {
        Self { group, name, run }
    }

    /// Runs the case to completion. Cases are repeatable.
    pub fn run(&self) -> BoxSpanFuture<'static, ()> {
        (self.run)()
    }
}

type SharedBody =
    Arc<dyn Fn(Box<dyn TelemetryAdapterFixture>) -> BoxSpanFuture<'static, ()> + Send + Sync>;

/// Builds one case, wiring a reusable body to a shared fixture factory.
pub(crate) fn case<F, Fut>(
    factory: &TelemetryAdapterFixtureFactory,
    group: &'static str,
    name: &'static str,
    body: F,
) -> TelemetryAdapterConformanceCase
where
    F: Fn(Box<dyn TelemetryAdapterFixture>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let shared_body: SharedBody =
        Arc::new(move |fixture| Box::pin(body(fixture)) as BoxSpanFuture<'static, ()>);
    let factory = Arc::clone(factory);
    TelemetryAdapterConformanceCase::new(
        group,
        name,
        Arc::new(move || {
            let factory = Arc::clone(&factory);
            let body = Arc::clone(&shared_body);
            Box::pin(async move {
                let fixture = factory().await;
                body(fixture).await;
            }) as BoxSpanFuture<'static, ()>
        }),
    )
}
