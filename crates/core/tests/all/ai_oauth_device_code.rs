//! Port of `pi-core/ai/test/oauth-device-code.test.ts`. The TypeScript suite
//! drives fake timers; these tests run under `start_paused` tokio time, which
//! auto-advances to the next sleep exactly like `vi.advanceTimersByTimeAsync`.

use std::time::Duration;

use pi_core::ai::auth::oauth::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use tokio_util::sync::CancellationToken;

fn never_cancelled() -> CancellationToken {
    CancellationToken::new()
}

/// Poll closure that records the elapsed (paused) time of each call and
/// replays scripted outcomes.
fn scripted_poll(
    start: tokio::time::Instant,
    times: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
    outcomes: std::sync::Arc<std::sync::Mutex<Vec<OAuthDeviceCodePollResult<String>>>>,
) -> impl Fn() -> std::future::Ready<OAuthDeviceCodePollResult<String>> {
    move || {
        times.lock().unwrap().push(start.elapsed());
        let outcome = outcomes.lock().unwrap().remove(0);
        std::future::ready(outcome)
    }
}

fn pending<T>() -> OAuthDeviceCodePollResult<T> {
    OAuthDeviceCodePollResult::Pending
}

#[tokio::test(start_paused = true)]
async fn polls_immediately_and_returns_the_completed_value() {
    let start = tokio::time::Instant::now();
    let times = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let poll = scripted_poll(
        start,
        std::sync::Arc::clone(&times),
        std::sync::Arc::new(std::sync::Mutex::new(vec![
            pending(),
            OAuthDeviceCodePollResult::Complete("token".to_string()),
        ])),
    );

    let result = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(2.0),
        expires_in_seconds: Some(30.0),
        wait_before_first_poll: false,
        signal: never_cancelled(),
        poll,
    })
    .await
    .unwrap();

    assert_eq!(result, "token");
    assert_eq!(
        *times.lock().unwrap(),
        vec![Duration::from_secs(0), Duration::from_secs(2)]
    );
}

#[tokio::test(start_paused = true)]
async fn can_wait_before_the_first_poll() {
    let start = tokio::time::Instant::now();
    let times = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let poll = scripted_poll(
        start,
        std::sync::Arc::clone(&times),
        std::sync::Arc::new(std::sync::Mutex::new(vec![
            OAuthDeviceCodePollResult::Complete("token".to_string()),
        ])),
    );

    let result = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(2.0),
        expires_in_seconds: Some(30.0),
        wait_before_first_poll: true,
        signal: never_cancelled(),
        poll,
    })
    .await
    .unwrap();

    assert_eq!(result, "token");
    // The first poll only happens after one full interval.
    assert_eq!(*times.lock().unwrap(), vec![Duration::from_secs(2)]);
}

#[tokio::test(start_paused = true)]
async fn increases_the_interval_by_five_seconds_after_slow_down_without_a_server_interval() {
    let start = tokio::time::Instant::now();
    let times = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let poll = scripted_poll(
        start,
        std::sync::Arc::clone(&times),
        std::sync::Arc::new(std::sync::Mutex::new(vec![
            OAuthDeviceCodePollResult::SlowDown {
                interval_seconds: None,
            },
            OAuthDeviceCodePollResult::Complete("token".to_string()),
        ])),
    );

    let result = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(2.0),
        expires_in_seconds: Some(900.0),
        wait_before_first_poll: false,
        signal: never_cancelled(),
        poll,
    })
    .await
    .unwrap();

    assert_eq!(result, "token");
    // 2s initial interval + 5s slow-down increment.
    assert_eq!(
        *times.lock().unwrap(),
        vec![Duration::from_secs(0), Duration::from_secs(7)]
    );
}

#[tokio::test(start_paused = true)]
async fn honors_a_server_provided_slow_down_interval() {
    let start = tokio::time::Instant::now();
    let times = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let poll = scripted_poll(
        start,
        std::sync::Arc::clone(&times),
        std::sync::Arc::new(std::sync::Mutex::new(vec![
            OAuthDeviceCodePollResult::SlowDown {
                interval_seconds: Some(30.0),
            },
            OAuthDeviceCodePollResult::Complete("token".to_string()),
        ])),
    );

    let result = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(2.0),
        expires_in_seconds: Some(900.0),
        wait_before_first_poll: false,
        signal: never_cancelled(),
        poll,
    })
    .await
    .unwrap();

    assert_eq!(result, "token");
    assert_eq!(
        *times.lock().unwrap(),
        vec![Duration::from_secs(0), Duration::from_secs(30)]
    );
}

#[tokio::test(start_paused = true)]
async fn cancels_an_in_flight_wait() {
    let signal = CancellationToken::new();
    let poll = || std::future::ready(pending::<String>());

    let flow = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(5.0),
        expires_in_seconds: Some(30.0),
        wait_before_first_poll: false,
        signal: signal.clone(),
        poll,
    });

    // Cancel while the flow is sleeping between polls.
    let cancel_after = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(cancel_after);
    tokio::select! {
        _ = &mut cancel_after => signal.cancel(),
        result = flow => {
            // If the flow finished first it must be with the cancel message.
            assert_eq!(result.unwrap_err(), "Login cancelled");
            return;
        }
    }

    let error = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(5.0),
        expires_in_seconds: Some(30.0),
        wait_before_first_poll: false,
        signal,
        poll: || std::future::ready(pending::<String>()),
    })
    .await
    .unwrap_err();
    assert_eq!(error, "Login cancelled");
}

#[tokio::test(start_paused = true)]
async fn times_out_with_the_slow_down_message_after_slow_down_responses() {
    let error = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(1.0),
        expires_in_seconds: Some(3.0),
        wait_before_first_poll: false,
        signal: never_cancelled(),
        poll: || {
            std::future::ready(OAuthDeviceCodePollResult::<()>::SlowDown {
                interval_seconds: None,
            })
        },
    })
    .await
    .unwrap_err();
    assert!(
        error.starts_with("Device flow timed out after one or more slow_down responses"),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn times_out_with_the_plain_message_without_slow_down_responses() {
    let error = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(1.0),
        expires_in_seconds: Some(2.0),
        wait_before_first_poll: false,
        signal: never_cancelled(),
        poll: || std::future::ready(OAuthDeviceCodePollResult::<()>::Pending),
    })
    .await
    .unwrap_err();
    assert_eq!(error, "Device flow timed out");
}

#[tokio::test(start_paused = true)]
async fn failed_polls_reject_with_their_message() {
    let error = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval_seconds: Some(1.0),
        expires_in_seconds: Some(30.0),
        wait_before_first_poll: false,
        signal: never_cancelled(),
        poll: || {
            std::future::ready(OAuthDeviceCodePollResult::<()>::Failed {
                message: "access_denied".to_string(),
            })
        },
    })
    .await
    .unwrap_err();
    assert_eq!(error, "access_denied");
}
