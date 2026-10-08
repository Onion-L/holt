//! Port of `pi-core/ai/src/utils/abort.ts` and `utils/abort-signals.ts`.
//!
//! TypeScript threads `AbortSignal`/`AbortController` through every provider
//! request; the Rust port uses `tokio_util::sync::CancellationToken`, which
//! composes the same way (child tokens mirror `AbortSignal.any`). Cancellation
//! carries no value: TS abort reasons map to the error values produced at the
//! await sites (typically the message "The operation was aborted").

use std::future::Future;

use tokio_util::sync::CancellationToken;

/// The default abort message used where TypeScript raises `AbortError`.
pub const ABORTED_MESSAGE: &str = "The operation was aborted";

/// Port of `operationSignal`: returns the given token or a fresh
/// never-cancelled one for operations whose signal is optional.
pub fn operation_signal(signal: Option<&CancellationToken>) -> CancellationToken {
    signal.cloned().unwrap_or_default()
}

/// Error produced when an operation loses an abort race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortRace {
    Aborted { message: String },
}

impl std::fmt::Display for AbortRace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbortRace::Aborted { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AbortRace {}

/// Error of [`race_with_abort_signal`]: either the operation's own error or
/// the abort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RaceError<E> {
    Operation(E),
    Aborted { message: String },
}

impl<E: std::fmt::Display> std::fmt::Display for RaceError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RaceError::Operation(error) => write!(f, "{error}"),
            RaceError::Aborted { message } => write!(f, "{message}"),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for RaceError<E> {}

/// Port of `raceWithAbortSignal`: resolves when the operation completes, or
/// rejects with the abort reason when the token cancels first.
///
/// Unlike a JavaScript promise, dropping the losing side of the race cancels
/// it; call sites that must keep the operation running spawn it separately.
pub async fn race_with_abort_signal<T, E, Fut>(
    operation: Fut,
    signal: &CancellationToken,
) -> Result<T, RaceError<E>>
where
    Fut: Future<Output = Result<T, E>>,
{
    tokio::select! {
        result = operation => result.map_err(RaceError::Operation),
        _ = signal.cancelled() => Err(RaceError::Aborted { message: ABORTED_MESSAGE.to_string() }),
    }
}

/// Aborts the current future when the token cancels, mirroring an awaited
/// `AbortSignal.throwIfAborted()` checkpoint.
pub async fn throw_if_aborted(signal: &CancellationToken) -> Result<(), AbortRace> {
    if signal.is_cancelled() {
        return Err(AbortRace::Aborted {
            message: ABORTED_MESSAGE.to_string(),
        });
    }
    Ok(())
}

/// Port of `combineAbortSignals` (from `utils/abort-signals.ts`).
///
/// Returns a combined token that cancels when any input cancels. With zero
/// active inputs there is no signal; with exactly one, that signal is
/// returned unchanged (no watcher task). Drop the returned guard to release
/// the watcher tasks, mirroring the TypeScript `cleanup` callback.
pub struct CombinedAbortSignal {
    pub signal: Option<CancellationToken>,
    watchers: Vec<tokio::task::JoinHandle<()>>,
}

impl CombinedAbortSignal {
    /// The combined signal, if any input was active.
    pub fn signal(&self) -> Option<&CancellationToken> {
        self.signal.as_ref()
    }
}

impl Drop for CombinedAbortSignal {
    fn drop(&mut self) {
        for watcher in self.watchers.drain(..) {
            watcher.abort();
        }
    }
}

/// Combines optional cancellation tokens, mirroring
/// `combineAbortSignals([...])`.
pub fn combine_abort_signals(
    signals: impl IntoIterator<Item = Option<CancellationToken>>,
) -> CombinedAbortSignal {
    let active: Vec<CancellationToken> = signals.into_iter().flatten().collect();
    if active.is_empty() {
        return CombinedAbortSignal {
            signal: None,
            watchers: Vec::new(),
        };
    }
    if active.len() == 1 {
        return CombinedAbortSignal {
            signal: Some(active[0].clone()),
            watchers: Vec::new(),
        };
    }

    let combined = CancellationToken::new();
    let mut watchers = Vec::with_capacity(active.len());
    for signal in active {
        if signal.is_cancelled() {
            combined.cancel();
            break;
        }
        let token = combined.clone();
        watchers.push(tokio::spawn(async move {
            signal.cancelled().await;
            token.cancel();
        }));
    }

    CombinedAbortSignal {
        signal: Some(combined),
        watchers,
    }
}

/// Port of `sleep(ms, signal)` from `utils/sleep.ts`: resolves after `ms`, or
/// rejects when the token cancels first.
pub async fn abortable_sleep(ms: u64, signal: Option<&CancellationToken>) -> Result<(), AbortRace> {
    match signal {
        Some(signal) => {
            if signal.is_cancelled() {
                return Err(AbortRace::Aborted {
                    message: ABORTED_MESSAGE.to_string(),
                });
            }
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(ms)) => Ok(()),
                _ = signal.cancelled() => Err(AbortRace::Aborted {
                    message: ABORTED_MESSAGE.to_string(),
                }),
            }
        }
        None => {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(())
        }
    }
}
