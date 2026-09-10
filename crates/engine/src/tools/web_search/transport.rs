//! Shared transport scaffolding for the backend adapters (ADR-0023):
//! the whole-exchange budget, the trait-side cancellation race, and the
//! common error-detail rendering. Each adapter owns its request shape,
//! auth header, response decoding, and hit mapping — this module is the
//! plumbing every adapter would otherwise copy.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::tools::USER_AGENT;

/// Send one request under a total wall-clock budget — request through
/// the last body byte (the web_fetch precedent). `build` receives the
/// client (holt User-Agent already set) and returns the fully-built
/// request, so GET-with-params and POST-with-JSON adapters share the
/// same bounds. `backend` names the service in every error.
pub(super) async fn send_bounded(
    backend: &'static str,
    timeout: Duration,
    build: impl FnOnce(reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<(reqwest::StatusCode, Vec<u8>), String> {
    let exchange = async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| format!("{backend} search could not start: {error}"))?;
        let response = build(client)
            .send()
            .await
            .map_err(|error| format!("{backend} search request failed: {error}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| format!("{backend} search failed reading the response: {error}"))?;
        Ok((status, body.to_vec()))
    };
    tokio::time::timeout(timeout, exchange).await.map_err(|_| {
        format!(
            "{backend} search timed out after {:.0}s",
            timeout.as_secs_f64()
        )
    })?
}

/// The trait-side race: the request future carries its own timeout;
/// dropping it on cancellation aborts the in-flight HTTP call.
pub(super) async fn race_cancel<T>(
    request: impl std::future::Future<Output = Result<T, String>>,
    cancel: CancellationToken,
) -> Result<T, String> {
    tokio::select! {
        _ = cancel.cancelled() => Err("search cancelled".to_string()),
        result = request => result,
    }
}

/// Render an API error body's `code`/`message` pair as the suffix of a
/// tool error — the shape every adapter's failures use after the HTTP
/// status. Empty pieces drop out; both empty renders nothing.
pub(super) fn error_detail(code: Option<String>, message: Option<String>) -> String {
    match (
        code.filter(|code| !code.is_empty()),
        message.filter(|message| !message.is_empty()),
    ) {
        (Some(code), Some(message)) => format!(" (code {code}): {message}"),
        (Some(code), None) => format!(" (code {code})"),
        (None, Some(message)) => format!(": {message}"),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_http::{response, serve};

    #[test]
    fn error_detail_renders_every_piece_combination() {
        assert_eq!(
            error_detail(Some("RATE_LIMITED".into()), Some("slow down".into())),
            " (code RATE_LIMITED): slow down"
        );
        assert_eq!(error_detail(Some("1301".into()), None), " (code 1301)");
        assert_eq!(error_detail(None, Some("boom".into())), ": boom");
        assert_eq!(error_detail(None, None), "");
        assert_eq!(
            error_detail(Some("".into()), Some("".into())),
            "",
            "empty pieces render nothing"
        );
    }

    #[tokio::test]
    async fn send_bounded_returns_status_and_body() {
        let server = serve(|_, _| response("200 OK", "application/json", br#"{"ok":true}"#)).await;

        let (status, body) = send_bounded("Stub", Duration::from_secs(5), |client| {
            client.get(format!("{}/x", server.base))
        })
        .await
        .unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(body, br#"{"ok":true}"#.to_vec());
    }

    #[tokio::test]
    async fn send_bounded_names_the_backend_on_every_failure() {
        // A hanging server under a short budget: the timeout error.
        let server = serve(|_, _| Vec::new()).await;
        let error = send_bounded("Stub", Duration::from_millis(100), |client| {
            client.get(format!("{}/hang", server.base))
        })
        .await
        .unwrap_err();
        assert!(
            error.contains("Stub search timed out after"),
            "unexpected: {error}"
        );

        // A refused connection: the request error.
        let error = send_bounded("Stub", Duration::from_secs(5), |client| {
            client.get("http://127.0.0.1:1/")
        })
        .await
        .unwrap_err();
        assert!(
            error.contains("Stub search request failed"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn race_cancel_returns_cancelled_and_passes_results_through() {
        let cancel = CancellationToken::new();
        let task_token = cancel.clone();
        let hanging: std::future::Pending<Result<u8, String>> = std::future::pending();
        let task = tokio::spawn(async move { race_cancel(hanging, task_token).await });
        cancel.cancel();
        assert_eq!(task.await.unwrap(), Err("search cancelled".to_string()));

        assert_eq!(
            race_cancel(async { Ok(7u8) }, CancellationToken::new()).await,
            Ok(7)
        );
    }
}
