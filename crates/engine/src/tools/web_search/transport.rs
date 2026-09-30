//! Shared transport scaffolding for the backend adapters (ADR-0023):
//! the whole-exchange budget, the response download cap, the trait-side
//! cancellation race, and the common error-detail rendering. Each adapter
//! owns its request shape, auth header, response decoding, and hit mapping
//! — this module is the plumbing every adapter would otherwise copy.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::tools::USER_AGENT;

/// Hard download cap for a backend response, checked against
/// `Content-Length` and streamed bytes — the web_fetch precedent: a
/// backend's reply is untrusted remote content, so it never enters
/// engine memory whole-and-unbounded.
const DOWNLOAD_BYTE_CAP: u64 = 5 * 1024 * 1024;
/// How many characters of a backend-supplied error string reach the tool
/// error — the whole error is replayed into the model's context, so an
/// oversized body must not arrive through the error path either.
const ERROR_DETAIL_CHARS: usize = 300;

/// Send one request under a total wall-clock budget — request through
/// the last body byte, with the body capped at [`DOWNLOAD_BYTE_CAP`]
/// (the web_fetch precedent). `build` receives the client (holt
/// User-Agent already set) and returns the fully-built request, so
/// GET-with-params and POST-with-JSON adapters share the same bounds.
/// `backend` names the service in every error.
pub(super) async fn send_bounded(
    backend: &str,
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
        if let Some(bytes) = response.content_length()
            && bytes > DOWNLOAD_BYTE_CAP
        {
            return Err(format!(
                "{backend} search failed: response is larger than the 5 MB download cap \
                 ({bytes} bytes declared by Content-Length)"
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                format!("{backend} search failed reading the response: {error}")
            })?;
            if body.len() as u64 + chunk.len() as u64 > DOWNLOAD_BYTE_CAP {
                return Err(format!(
                    "{backend} search failed: response is larger than the 5 MB download cap \
                     (stopped after {DOWNLOAD_BYTE_CAP} bytes)"
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
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
/// status. Empty pieces drop out; both empty renders nothing. Each
/// piece is clamped by [`clamp_error_text`].
pub(super) fn error_detail(code: Option<String>, message: Option<String>) -> String {
    match (
        code.filter(|code| !code.is_empty())
            .map(|code| clamp_error_text(&code)),
        message
            .filter(|message| !message.is_empty())
            .map(|message| clamp_error_text(&message)),
    ) {
        (Some(code), Some(message)) => format!(" (code {code}): {message}"),
        (Some(code), None) => format!(" (code {code})"),
        (None, Some(message)) => format!(": {message}"),
        (None, None) => String::new(),
    }
}

/// A backend-supplied error string clamped to [`ERROR_DETAIL_CHARS`] —
/// backend text is untrusted, so no adapter embeds it in a tool error
/// whole.
pub(super) fn clamp_error_text(text: &str) -> String {
    if text.chars().count() <= ERROR_DETAIL_CHARS {
        return text.to_string();
    }
    let mut clamped: String = text.chars().take(ERROR_DETAIL_CHARS).collect();
    clamped.push('…');
    clamped
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

    #[test]
    fn backend_error_pieces_are_clamped_not_embedded_whole() {
        let long = "e".repeat(ERROR_DETAIL_CHARS + 5_000);
        let detail = error_detail(Some("RATE_LIMITED".into()), Some(long));
        assert!(
            detail.len() < 2 * ERROR_DETAIL_CHARS,
            "error detail not clamped: {} chars",
            detail.chars().count()
        );
        assert!(detail.ends_with('…'), "missing truncation marker: {detail}");

        assert_eq!(clamp_error_text("short"), "short");
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
    async fn a_declared_oversize_body_is_rejected_before_streaming() {
        let server = serve(|_, _| {
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 6000000\r\nConnection: close\r\n\r\n"
                .to_vec()
        })
        .await;

        let error = send_bounded("Stub", Duration::from_secs(5), |client| {
            client.get(format!("{}/big", server.base))
        })
        .await
        .unwrap_err();
        assert!(
            error.contains("Stub search failed: response is larger than the 5 MB download cap"),
            "unexpected: {error}"
        );
        assert!(
            error.contains("6000000 bytes declared"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn a_streamed_oversize_body_is_rejected_mid_download() {
        let over = DOWNLOAD_BYTE_CAP + 1;
        let server = serve(move |_, _| {
            let mut out =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                    .to_vec();
            out.resize(out.len() + over as usize, b'a');
            out
        })
        .await;

        let error = send_bounded("Stub", Duration::from_secs(5), |client| {
            client.get(format!("{}/flood", server.base))
        })
        .await
        .unwrap_err();
        assert!(
            error.contains("Stub search failed: response is larger than the 5 MB download cap"),
            "unexpected: {error}"
        );
        assert!(error.contains("stopped after"), "unexpected: {error}");
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
