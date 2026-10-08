//! Port of `@earendil-works/pi-telemetry` v0.84.4
//! (`pi-core/telemetry/src`).
//!
//! Status and per-module mapping live in `MIGRATION.md` at the repository
//! root. The implementation is ported in this module's children; the public
//! surface mirrors the TypeScript package's `index.ts` and `testing` entry
//! point.
//!
//! # TypeScript-to-Rust API mapping
//!
//! The TypeScript contract is callback based: `startSpan(options, callback)`
//! admits the callback synchronously, the callback may return a value or a
//! promise, and the span settles when that result settles. Rust has no
//! anonymous promises, so every context and span exposes the same contract
//! through two dispatch flavors plus typed convenience wrappers:
//!
//! - a synchronous dispatch (`dispatch_span_sync`) for callbacks that return
//!   immediately, and
//! - an asynchronous dispatch (`dispatch_span_async`) for callbacks that
//!   return a future, with settlement deferred until the future completes.
//!
//! TypeScript rejections thrown from a callback map to `Result` errors in the
//! `try_*` convenience wrappers; the span settles with an automatic error
//! status unless a status was set explicitly, exactly as in TypeScript.
//! Panics are not caught: Rust panics indicate bugs rather than control flow.
//!
//! Error names in automatic span statuses use the short Rust type name of the
//! error (`std::any::type_name` minus its module path) where TypeScript uses
//! `error.name`. Error messages use [`std::fmt::Display`].

mod memory;
mod noop;
pub mod testing;

pub use memory::{InMemoryTelemetryContext, RecordedTelemetryEvent, RecordedTelemetrySpan};
pub use noop::{NOOP_TELEMETRY_CONTEXT, NoopTelemetryContext, noop_telemetry_context};

use std::any::type_name;
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

// ---------------------------------------------------------------------------
// Attribute and option types (port of the top of index.ts)
// ---------------------------------------------------------------------------

/// Port of `AttributeValue`: a single telemetry attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Boolean(bool),
    StringArray(Vec<String>),
    NumberArray(Vec<f64>),
    BooleanArray(Vec<bool>),
}

impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        AttributeValue::String(value.to_string())
    }
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        AttributeValue::String(value)
    }
}

impl From<f64> for AttributeValue {
    fn from(value: f64) -> Self {
        AttributeValue::Number(value)
    }
}

impl From<i64> for AttributeValue {
    fn from(value: i64) -> Self {
        AttributeValue::Number(value as f64)
    }
}

impl From<i32> for AttributeValue {
    fn from(value: i32) -> Self {
        AttributeValue::Number(f64::from(value))
    }
}

impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        AttributeValue::Boolean(value)
    }
}

impl From<Vec<String>> for AttributeValue {
    fn from(value: Vec<String>) -> Self {
        AttributeValue::StringArray(value)
    }
}

impl From<Vec<f64>> for AttributeValue {
    fn from(value: Vec<f64>) -> Self {
        AttributeValue::NumberArray(value)
    }
}

impl From<Vec<bool>> for AttributeValue {
    fn from(value: Vec<bool>) -> Self {
        AttributeValue::BooleanArray(value)
    }
}

/// Port of `SpanAttributes`.
///
/// TypeScript attributes are objects whose values may be `undefined`;
/// `undefined` entries are skipped by the recorder. Rust values cannot be
/// `undefined`, so callers simply omit unwanted keys, which produces the same
/// recorded state. Keys are stored in a [`BTreeMap`] for deterministic
/// iteration; TypeScript object key order is not part of the telemetry
/// contract.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanAttributes(BTreeMap<String, AttributeValue>);

impl SpanAttributes {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Builder-style insertion, mirroring object-literal construction.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<AttributeValue>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<AttributeValue>) {
        self.0.insert(name.into(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&AttributeValue> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &AttributeValue)> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<const N: usize> From<[(&str, AttributeValue); N]> for SpanAttributes {
    fn from(entries: [(&str, AttributeValue); N]) -> Self {
        entries
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }
}

impl FromIterator<(String, AttributeValue)> for SpanAttributes {
    fn from_iter<T: IntoIterator<Item = (String, AttributeValue)>>(entries: T) -> Self {
        Self(entries.into_iter().collect())
    }
}

impl IntoIterator for SpanAttributes {
    type Item = (String, AttributeValue);
    type IntoIter = std::collections::btree_map::IntoIter<String, AttributeValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Port of `SpanOptions`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: SpanAttributes,
}

impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: SpanAttributes::new(),
        }
    }

    /// Builder-style attribute insertion.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<AttributeValue>) -> Self {
        self.attributes.set(name, value);
        self
    }

    /// Replaces the attribute set wholesale.
    pub fn with_attributes(mut self, attributes: SpanAttributes) -> Self {
        self.attributes = attributes;
        self
    }
}

/// Name and message pair recorded in an error span status. Port of the
/// `{ name, message }` payload of TypeScript `SpanStatus`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanErrorInfo {
    pub name: String,
    pub message: String,
}

impl SpanErrorInfo {
    /// Builds the automatic error status payload for a callback failure.
    ///
    /// TypeScript reads `error.name` and `error.message`; Rust errors carry
    /// no name field, so the short type name of the error is used instead.
    pub fn for_error<E: std::fmt::Display>(error: &E) -> Self {
        Self {
            name: short_type_name::<E>(),
            message: error.to_string(),
        }
    }
}

/// Port of `SpanStatus`: `{ status: "ok" } | { status: "error", error? }`.
#[derive(Clone, Debug, PartialEq)]
pub enum SpanStatus {
    Ok,
    Error(Option<SpanErrorInfo>),
}

fn short_type_name<T: ?Sized>() -> String {
    let full = type_name::<T>();
    full.rsplit("::").next().unwrap_or(full).to_string()
}

/// Locks a mutex, treating poisoning as recoverable. Telemetry recording is
/// passive in the TypeScript source and must never turn a poisoned lock into
/// a caller-visible failure.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Context and span contracts (port of TelemetryContext / TelemetrySpan)
// ---------------------------------------------------------------------------

/// Boxed future produced by asynchronous span dispatch.
pub type BoxSpanFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Boxed synchronous span body. `Err` marks the span as failed and supplies
/// the automatic error status payload.
pub type SpanBodySync<'a> =
    Box<dyn FnOnce(Arc<dyn TelemetrySpan>) -> Result<(), SpanErrorInfo> + 'a>;

/// Boxed asynchronous span body; the span settles when the future settles.
pub type SpanBodyAsync<'a> = Box<
    dyn FnOnce(Arc<dyn TelemetrySpan>) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>> + Send + 'a,
>;

/// Port of `TelemetrySpan` (the mutable span handle handed to callbacks).
///
/// Handles are shared references (`Arc<dyn TelemetrySpan>`), matching
/// TypeScript's object semantics: a callback may retain the handle and call
/// recording methods after its span has settled; such calls are inert.
pub trait TelemetrySpan: Send + Sync {
    /// Port of `addEvent`.
    fn add_event(&self, name: &str, attributes: SpanAttributes);

    /// Port of `setAttributes`.
    fn set_attributes(&self, attributes: SpanAttributes);

    /// Port of `setStatus`.
    fn set_status(&self, status: SpanStatus);

    /// Object-safe synchronous dispatch for child spans, mirroring
    /// `startSpan` with a synchronous callback.
    fn dispatch_child_span_sync(
        &self,
        options: SpanOptions,
        body: SpanBodySync<'_>,
    ) -> Result<(), SpanErrorInfo>;

    /// Object-safe asynchronous dispatch for child spans, mirroring
    /// `startSpan` with a promise-returning callback.
    fn dispatch_child_span_async<'a>(
        &'a self,
        options: SpanOptions,
        body: SpanBodyAsync<'a>,
    ) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>>;
}

/// Port of `TelemetryContext`.
///
/// Implementations own span lifecycle. The two `dispatch_*` methods form the
/// object-safe core used by [`Arc<dyn TelemetryContext>`]; the
/// [`TelemetryContextExt`] extension trait supplies the typed convenience
/// wrappers used by application code and tests.
pub trait TelemetryContext: Send + Sync {
    /// Object-safe synchronous dispatch, mirroring `startSpan` with a
    /// synchronous callback.
    fn dispatch_span_sync(
        &self,
        options: SpanOptions,
        body: SpanBodySync<'_>,
    ) -> Result<(), SpanErrorInfo>;

    /// Object-safe asynchronous dispatch, mirroring `startSpan` with a
    /// promise-returning callback.
    fn dispatch_span_async<'a>(
        &'a self,
        options: SpanOptions,
        body: SpanBodyAsync<'a>,
    ) -> BoxSpanFuture<'a, Result<(), SpanErrorInfo>>;
}

/// Typed wrappers over the object-safe dispatch core, preserving the
/// TypeScript `startSpan(options, callback)` contract: synchronous admission,
/// result preservation, and error-status settlement on failure.
pub trait TelemetryContextExt: TelemetryContext {
    /// Runs an infallible synchronous callback inside a new span.
    fn start_span<T>(
        &self,
        options: SpanOptions,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>) -> T,
    ) -> T {
        let mut slot: Option<T> = None;
        let _ = self.dispatch_span_sync(
            options,
            Box::new(|span| {
                slot = Some(callback(span));
                Ok(())
            }),
        );
        slot.expect("synchronous span body must run exactly once")
    }

    /// Runs a fallible synchronous callback inside a new span. On `Err` the
    /// span settles with an automatic error status (unless a status was set
    /// explicitly) and the error is returned, mirroring a TypeScript throw.
    fn try_start_span<T, E: std::fmt::Display>(
        &self,
        options: SpanOptions,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut slot: Option<Result<T, E>> = None;
        let _ = self.dispatch_span_sync(
            options,
            Box::new(|span| {
                let value = callback(span);
                let failure = match &value {
                    Ok(_) => None,
                    Err(error) => Some(SpanErrorInfo::for_error(error)),
                };
                slot = Some(value);
                match failure {
                    Some(info) => Err(info),
                    None => Ok(()),
                }
            }),
        );
        slot.expect("synchronous span body must run exactly once")
    }

    /// Runs an infallible asynchronous callback inside a new span; the span
    /// settles when the returned future completes.
    fn start_span_async<'a, T, F, Fut>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> BoxSpanFuture<'a, T>
    where
        T: Send + 'a,
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
    {
        let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let inner_slot = Arc::clone(&slot);
        let inner = self.dispatch_span_async(
            options,
            Box::new(move |span| {
                let slot = inner_slot;
                let fut = callback(span);
                Box::pin(async move {
                    let value = fut.await;
                    *lock(&slot) = Some(value);
                    Ok(())
                })
            }),
        );
        Box::pin(async move {
            let _ = inner.await;
            lock(&slot)
                .take()
                .expect("asynchronous span body must run exactly once")
        })
    }

    /// Runs a fallible asynchronous callback inside a new span. On `Err` the
    /// span settles with an automatic error status (unless a status was set
    /// explicitly) and the error is returned, mirroring a rejected promise.
    fn try_start_span_async<'a, T, E, F, Fut>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> BoxSpanFuture<'a, Result<T, E>>
    where
        T: Send + 'a,
        E: std::fmt::Display + Send + 'a,
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = Result<T, E>> + Send + 'a,
    {
        let slot: Arc<Mutex<Option<Result<T, E>>>> = Arc::new(Mutex::new(None));
        let inner_slot = Arc::clone(&slot);
        let inner = self.dispatch_span_async(
            options,
            Box::new(move |span| {
                let slot = inner_slot;
                let fut = callback(span);
                Box::pin(async move {
                    let value = fut.await;
                    let failure = match &value {
                        Ok(_) => None,
                        Err(error) => Some(SpanErrorInfo::for_error(error)),
                    };
                    *lock(&slot) = Some(value);
                    match failure {
                        Some(info) => Err(info),
                        None => Ok(()),
                    }
                })
            }),
        );
        Box::pin(async move {
            let _ = inner.await;
            lock(&slot)
                .take()
                .expect("asynchronous span body must run exactly once")
        })
    }
}

impl<C: TelemetryContext + ?Sized> TelemetryContextExt for C {}

/// Typed wrappers over [`TelemetrySpan::dispatch_child_span_sync`] and
/// [`TelemetrySpan::dispatch_child_span_async`], mirroring that a
/// `TelemetrySpan` is itself a `TelemetryContext` in TypeScript.
pub trait TelemetrySpanExt: TelemetrySpan {
    /// Runs an infallible synchronous callback inside a child span.
    fn start_span<T>(
        &self,
        options: SpanOptions,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>) -> T,
    ) -> T {
        let mut slot: Option<T> = None;
        let _ = self.dispatch_child_span_sync(
            options,
            Box::new(|span| {
                slot = Some(callback(span));
                Ok(())
            }),
        );
        slot.expect("synchronous span body must run exactly once")
    }

    /// Runs a fallible synchronous callback inside a child span.
    fn try_start_span<T, E: std::fmt::Display>(
        &self,
        options: SpanOptions,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut slot: Option<Result<T, E>> = None;
        let _ = self.dispatch_child_span_sync(
            options,
            Box::new(|span| {
                let value = callback(span);
                let failure = match &value {
                    Ok(_) => None,
                    Err(error) => Some(SpanErrorInfo::for_error(error)),
                };
                slot = Some(value);
                match failure {
                    Some(info) => Err(info),
                    None => Ok(()),
                }
            }),
        );
        slot.expect("synchronous span body must run exactly once")
    }

    /// Runs an infallible asynchronous callback inside a child span.
    fn start_span_async<'a, T, F, Fut>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> BoxSpanFuture<'a, T>
    where
        T: Send + 'a,
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
    {
        let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let inner_slot = Arc::clone(&slot);
        let inner = self.dispatch_child_span_async(
            options,
            Box::new(move |span| {
                let slot = inner_slot;
                let fut = callback(span);
                Box::pin(async move {
                    let value = fut.await;
                    *lock(&slot) = Some(value);
                    Ok(())
                })
            }),
        );
        Box::pin(async move {
            let _ = inner.await;
            lock(&slot)
                .take()
                .expect("asynchronous span body must run exactly once")
        })
    }

    /// Runs a fallible asynchronous callback inside a child span.
    fn try_start_span_async<'a, T, E, F, Fut>(
        &'a self,
        options: SpanOptions,
        callback: F,
    ) -> BoxSpanFuture<'a, Result<T, E>>
    where
        T: Send + 'a,
        E: std::fmt::Display + Send + 'a,
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = Result<T, E>> + Send + 'a,
    {
        let slot: Arc<Mutex<Option<Result<T, E>>>> = Arc::new(Mutex::new(None));
        let inner_slot = Arc::clone(&slot);
        let inner = self.dispatch_child_span_async(
            options,
            Box::new(move |span| {
                let slot = inner_slot;
                let fut = callback(span);
                Box::pin(async move {
                    let value = fut.await;
                    let failure = match &value {
                        Ok(_) => None,
                        Err(error) => Some(SpanErrorInfo::for_error(error)),
                    };
                    *lock(&slot) = Some(value);
                    match failure {
                        Some(info) => Err(info),
                        None => Ok(()),
                    }
                })
            }),
        );
        Box::pin(async move {
            let _ = inner.await;
            lock(&slot)
                .take()
                .expect("asynchronous span body must run exactly once")
        })
    }
}

impl<S: TelemetrySpan + ?Sized> TelemetrySpanExt for S {}

// ---------------------------------------------------------------------------
// Schema definition types (port of the schema portion of index.ts)
// ---------------------------------------------------------------------------

/// Port of `TelemetryAttributeType`.
pub type TelemetryAttributeType = TelemetryAttributeKind;

/// Port of `TelemetryAttributeMetadata.cardinality`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryCardinality {
    Low,
    High,
}

/// Port of `TelemetryAttributeDefinition`. The `type`-tagged union becomes a
/// flattened enum; `values`/`examples` travel with each variant exactly as in
/// the TypeScript shape.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelemetryAttributeDefinition {
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cardinality: Option<TelemetryCardinality>,
    #[serde(flatten)]
    pub kind: TelemetryAttributeKind,
}

/// The `type`-tagged member of `TelemetryAttributeDefinition`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum TelemetryAttributeKind {
    #[serde(rename = "string")]
    String {
        #[serde(skip_serializing_if = "Option::is_none")]
        values: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        examples: Option<Vec<String>>,
    },
    #[serde(rename = "number")]
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        values: Option<Vec<f64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        examples: Option<Vec<f64>>,
    },
    #[serde(rename = "boolean")]
    Boolean {
        #[serde(skip_serializing_if = "Option::is_none")]
        values: Option<Vec<bool>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        examples: Option<Vec<bool>>,
    },
    #[serde(rename = "string[]")]
    StringArray {
        #[serde(rename = "elementValues", skip_serializing_if = "Option::is_none")]
        element_values: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        examples: Option<Vec<Vec<String>>>,
    },
    #[serde(rename = "number[]")]
    NumberArray {
        #[serde(rename = "elementValues", skip_serializing_if = "Option::is_none")]
        element_values: Option<Vec<f64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        examples: Option<Vec<Vec<f64>>>,
    },
    #[serde(rename = "boolean[]")]
    BooleanArray {
        #[serde(rename = "elementValues", skip_serializing_if = "Option::is_none")]
        element_values: Option<Vec<bool>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        examples: Option<Vec<Vec<bool>>>,
    },
}

/// Port of `TelemetryStartAttributeDefinition` (also used for event
/// attributes, which share the same shape).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelemetryStartAttributeDefinition {
    #[serde(flatten)]
    pub attribute: TelemetryAttributeDefinition,
    pub required: bool,
}

/// Port of `TelemetryEventAttributeDefinition`.
pub type TelemetryEventAttributeDefinition = TelemetryStartAttributeDefinition;

/// Port of `TelemetryEventDefinition`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelemetryEventDefinition {
    pub description: String,
    pub attributes: BTreeMap<String, TelemetryEventAttributeDefinition>,
}

/// Port of `TelemetryParentDefinition`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryParentDefinition {
    Any,
    RootOrExternal,
    Spans { spans: Vec<String> },
}

/// Port of the `status` member of `TelemetrySpanDefinition`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelemetrySpanStatusDefinition {
    #[serde(rename = "default")]
    pub default: TelemetryStatusDefault,
    #[serde(rename = "errorWhen")]
    pub error_when: String,
}

/// The literal `"ok"` default status of a span definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryStatusDefault {
    Ok,
}

/// Port of `TelemetrySpanDefinition`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelemetrySpanDefinition {
    pub description: String,
    pub parents: TelemetryParentDefinition,
    #[serde(rename = "startAttributes")]
    pub start_attributes: BTreeMap<String, TelemetryStartAttributeDefinition>,
    #[serde(rename = "endAttributes")]
    pub end_attributes: BTreeMap<String, TelemetryAttributeDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<BTreeMap<String, TelemetryEventDefinition>>,
    pub status: TelemetrySpanStatusDefinition,
}

/// Port of `TelemetrySchemaDefinition`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelemetrySchemaDefinition {
    pub version: u64,
    pub spans: BTreeMap<String, TelemetrySpanDefinition>,
}

/// Port of `defineTelemetrySchema`: a typed identity helper. Like the
/// TypeScript original, it performs no runtime validation.
pub fn define_telemetry_schema<T>(schema: T) -> T {
    schema
}

// ---------------------------------------------------------------------------
// Typed span starter (port of bindTypedSpanStarter / createTypedSpanStarter)
// ---------------------------------------------------------------------------

/// Port of the typed span starter bound to one parent context.
///
/// In TypeScript this type carries compile-time span vocabularies via
/// conditional types; those checks have no Rust equivalent and are documented
/// as a type-level-only capability. The runtime behavior — binding a context,
/// forwarding name and attributes, and handing callbacks a child starter — is
/// preserved.
pub struct TypedSpanStarter<Schemas> {
    context: Arc<dyn TelemetryContext>,
    _schemas: PhantomData<Schemas>,
}

/// Port of the child starter bound to one parent span.
pub struct TypedChildStarter<Schemas> {
    span: Arc<dyn TelemetrySpan>,
    _schemas: PhantomData<Schemas>,
}

impl<Schemas> TypedChildStarter<Schemas> {
    /// Starts a child span with a synchronous callback.
    pub fn start_span<T>(
        &self,
        name: &str,
        attributes: SpanAttributes,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>) -> T,
    ) -> T {
        self.span
            .start_span(SpanOptions::new(name).with_attributes(attributes), callback)
    }

    /// Starts a child span with a fallible synchronous callback.
    pub fn try_start_span<T, E: std::fmt::Display>(
        &self,
        name: &str,
        attributes: SpanAttributes,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>) -> Result<T, E>,
    ) -> Result<T, E> {
        self.span
            .try_start_span(SpanOptions::new(name).with_attributes(attributes), callback)
    }

    /// Starts a child span with an asynchronous callback.
    pub fn start_span_async<'a, T, F, Fut>(
        &'a self,
        name: &str,
        attributes: SpanAttributes,
        callback: F,
    ) -> BoxSpanFuture<'a, T>
    where
        T: Send + 'a,
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
    {
        self.span
            .start_span_async(SpanOptions::new(name).with_attributes(attributes), callback)
    }
}

impl<Schemas> TypedSpanStarter<Schemas> {
    /// Starts a span with a synchronous callback. The callback receives the
    /// span handle and a starter bound to that span for children.
    pub fn start_span<T>(
        &self,
        name: &str,
        attributes: SpanAttributes,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>, TypedChildStarter<Schemas>) -> T,
    ) -> T {
        self.context
            .start_span(SpanOptions::new(name).with_attributes(attributes), |span| {
                let child = TypedChildStarter {
                    span: Arc::clone(&span),
                    _schemas: PhantomData,
                };
                callback(span, child)
            })
    }

    /// Starts a span with a fallible synchronous callback.
    pub fn try_start_span<T, E: std::fmt::Display>(
        &self,
        name: &str,
        attributes: SpanAttributes,
        callback: impl FnOnce(Arc<dyn TelemetrySpan>, TypedChildStarter<Schemas>) -> Result<T, E>,
    ) -> Result<T, E> {
        self.context
            .try_start_span(SpanOptions::new(name).with_attributes(attributes), |span| {
                let child = TypedChildStarter {
                    span: Arc::clone(&span),
                    _schemas: PhantomData,
                };
                callback(span, child)
            })
    }

    /// Starts a span with a fallible asynchronous callback.
    pub fn try_start_span_async<'a, T, E, F, Fut>(
        &'a self,
        name: &str,
        attributes: SpanAttributes,
        callback: F,
    ) -> BoxSpanFuture<'a, Result<T, E>>
    where
        T: Send + 'a,
        E: std::fmt::Display + Send + 'a,
        F: FnOnce(Arc<dyn TelemetrySpan>, TypedChildStarter<Schemas>) -> Fut + Send + 'a,
        Fut: Future<Output = Result<T, E>> + Send + 'a,
    {
        self.context.try_start_span_async(
            SpanOptions::new(name).with_attributes(attributes),
            move |span| {
                let child = TypedChildStarter {
                    span: Arc::clone(&span),
                    _schemas: PhantomData,
                };
                callback(span, child)
            },
        )
    }
}

/// Port of `createTypedSpanStarter`. Schema values are used only to select
/// the `Schemas` marker; no runtime schema validation is performed, matching
/// the TypeScript original (including its tolerance of any schema value).
pub fn create_typed_span_starter<Schemas>(
    telemetry_context: Arc<dyn TelemetryContext>,
    _schemas: Schemas,
) -> TypedSpanStarter<Schemas> {
    TypedSpanStarter {
        context: telemetry_context,
        _schemas: PhantomData,
    }
}
