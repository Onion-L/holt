//! The Jev judge (ADR-0026): the external decision layer behind the
//! `jev-review` permission mode. One TypeSafe System One request per judged
//! mutating tool call — state carries only what the judgment needs (the
//! tool identity, its arguments, the working directory, and the user's
//! latest message), and the questions are a fixed set of atomic Nouls
//! combined by this module, never one wide "is this call OK?" question.
//!
//! Verdict mapping is conservative by construction: a clear veto denies,
//! a dead-band answer (neither clearly yes nor clearly no) escalates to
//! the user, and transport failures escalate too — the gate never fails
//! open. 429/529 retry with exponential backoff before that.
//!
//! The judge is harness-written, never a provider: the decision API has no
//! chat-completion shape, so this module speaks its single endpoint
//! directly with reqwest + serde and is injectable for tests through the
//! `JevJudge` trait (the web-search backend pattern).

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

/// The model the judge runs; pinned, not a setting (ADR-0026).
pub(crate) const MODEL: &str = "jev-latest";
/// The identity a judged call's usage record carries (ADR-0026).
pub(crate) const PROVIDER: &str = "typesafe";
const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// A Noul at or above this counts as its answer's "yes" (TypeSafe's own
/// action-threshold guidance).
const ACTION_THRESHOLD: f64 = 0.6;
/// At or below this a Noul counts as its answer's "no". The band in
/// between is the judge being unsure — the agreed overall-confidence
/// escalation gate, expressed on Noul probabilities because Nouls carry no
/// separate confidence field.
const DEAD_BAND_LOW: f64 = 0.4;
/// 429/529 retries before the call escalates (exponential backoff).
const RETRY_MAX: usize = 3;
const RETRY_BASE_MS: u64 = 250;
/// The judge must stay off the critical path: TypeSafe reports ~100ms
/// medians, and anything past this escalates rather than stalls the Turn.
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// What a judged call looks like to the judge.
pub struct JevCall<'a> {
    pub tool: &'a str,
    pub arguments: &'a serde_json::Value,
    pub cwd: &'a str,
    /// The user's latest message — the intent the call is judged against.
    pub request: &'a str,
}

/// The judge's verdict on one call.
#[derive(Debug, Clone, PartialEq)]
pub enum JevVerdict {
    Allow,
    Deny {
        reason: String,
    },
    /// The judge could not decide — the user decides.
    Unsure,
}

/// One judged call's full result: the verdict plus the reported token
/// counts, booked into the chat's usage ledger whatever the verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct JevJudgment {
    pub verdict: JevVerdict,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The injectable judge seam (the web-search backend pattern): production
/// mounts the HTTP client over the settings record; tests mount stubs.
pub trait JevJudge: Send + Sync {
    fn judge<'a>(
        &'a self,
        call: JevCall<'a>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JevJudgment, String>>;
}

/// The answers the fixed Noul set returns, one field per question.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct JevAnswers {
    pub(crate) destructive: f64,
    pub(crate) secrets: f64,
    pub(crate) matches_request: f64,
    pub(crate) needs_human: f64,
}

/// Combine the fixed Noul set into a verdict. Pure — unit-tested. Veto
/// questions deny at full strength; a clear irrelevance denies; anything
/// landing in the dead band, or the ask-the-user question firing,
/// escalates. Order matters: a clear veto outranks uncertainty elsewhere.
pub(crate) fn combine(a: &JevAnswers) -> JevVerdict {
    if a.destructive >= ACTION_THRESHOLD {
        return JevVerdict::Deny {
            reason: "the call looks destructive or irreversible".into(),
        };
    }
    if a.secrets >= ACTION_THRESHOLD {
        return JevVerdict::Deny {
            reason: "the call may expose credentials or private data".into(),
        };
    }
    if a.matches_request <= DEAD_BAND_LOW {
        return JevVerdict::Deny {
            reason: "the call does not appear to advance the user's request".into(),
        };
    }
    if a.needs_human >= ACTION_THRESHOLD {
        return JevVerdict::Unsure;
    }
    let dead_band = |n: f64| n > DEAD_BAND_LOW && n < ACTION_THRESHOLD;
    if dead_band(a.destructive)
        || dead_band(a.secrets)
        || dead_band(a.needs_human)
        || dead_band(a.matches_request)
    {
        return JevVerdict::Unsure;
    }
    JevVerdict::Allow
}

/// The fixed question set: four atomic Nouls over the call. Ids are
/// addressing only — the model never sees them, so each instruction is
/// self-contained and points into `state` with backtick paths.
fn questions() -> serde_json::Value {
    serde_json::json!({
        "destructive": {
            "type": "noul",
            "instructions": "Is `state.tool_call` destructive or irreversible — if it turned out to be wrong, would deleted files, dropped data, force-pushed refs, or overwritten state outside the repository be unrecoverable?",
            "criteria": {
                "true": "The call deletes, force-overwrites, or destroys data or state that re-running the task could not restore.",
                "false": "Whatever the call changes is ordinary, recoverable work — creating, editing, listing, or reading."
            }
        },
        "secrets": {
            "type": "noul",
            "instructions": "Does `state.tool_call` move credentials, API keys, or private user data off the local machine — into a remote request, an upload, a commit, or a public artifact?",
            "criteria": {
                "true": "The call transmits or persists a secret or private data somewhere a leak would matter.",
                "false": "The call touches no credentials or private data, or keeps them on the local machine."
            }
        },
        "matches_request": {
            "type": "noul",
            "instructions": "Does `state.tool_call` advance `state.request`, the user's latest message, rather than serving some unrelated goal?",
            "criteria": {
                "true": "The call is a plausible step in carrying out the user's request.",
                "false": "The call serves something else — or has no visible connection to the request."
            }
        },
        "needs_human": {
            "type": "noul",
            "instructions": "Does `state.tool_call` need the user's judgment before it runs — would its appropriateness depend on facts or intentions only the user knows?",
            "criteria": {
                "true": "A reasonable person would want to be asked first.",
                "false": "Nothing about the call calls for the user's personal judgment."
            }
        }
    })
}

/// The production judge: one HTTP exchange against the System One
/// endpoint per call, Bearer-authenticated with the user's own key.
pub struct JevClient {
    api_key: String,
}

impl JevClient {
    pub(crate) fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
        }
    }
}

impl JevJudge for JevClient {
    fn judge<'a>(
        &'a self,
        call: JevCall<'a>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JevJudgment, String>> {
        Box::pin(async move {
            let state = serde_json::json!({
                "request": call.request,
                "working_directory": call.cwd,
                "tool_call": {
                    "tool": call.tool,
                    "arguments": call.arguments,
                },
            });
            let body = serde_json::json!({
                "state": state,
                "model": MODEL,
                "questions": questions(),
            });
            // 429/529 back off and retry; everything else fails at once —
            // and every failure escalates at the gate, never opens it.
            let mut attempt = 0;
            loop {
                let response = http_post(ENDPOINT, &self.api_key, &body, &cancel).await;
                match response {
                    Ok(reply) => return parse_judgment(&reply),
                    Err(JevHttpError::Retryable(status)) => {
                        if attempt >= RETRY_MAX {
                            return Err(format!(
                                "the Jev judge stayed overloaded after {RETRY_MAX} retries (last status {status})"
                            ));
                        }
                        let delay =
                            std::time::Duration::from_millis(RETRY_BASE_MS * (1 << attempt));
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = cancel.cancelled() => {
                                return Err("the Jev judge was cancelled".into())
                            }
                        }
                        attempt += 1;
                    }
                    Err(JevHttpError::Fatal(error)) => return Err(error),
                }
            }
        })
    }
}

/// One POST exchange. A shared client keeps connection pooling across
/// admissions; the timeout bounds the gate's stay on the critical path.
async fn http_post(
    endpoint: &str,
    api_key: &str,
    body: &serde_json::Value,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, JevHttpError> {
    static CLIENT: std::sync::OnceLock<Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|error| format!("could not build the Jev HTTP client: {error}"))
    });
    let client = match client {
        Ok(client) => client,
        // A client that cannot even build fails loudly on every call — the
        // gate escalates, it never silently loses its timeout.
        Err(error) => return Err(JevHttpError::Fatal(error.clone())),
    };
    let request = async {
        client
            .post(endpoint)
            .bearer_auth(api_key)
            .json(body)
            .send()
            .await
    };
    tokio::select! {
        response = request => match response {
            Ok(response) => match response.status().as_u16() {
                200 => response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|error| JevHttpError::Fatal(format!("the Jev judge returned an unreadable body: {error}"))),
                429 | 529 => Err(JevHttpError::Retryable(response.status().as_u16())),
                status => Err(JevHttpError::Fatal(format!(
                    "the Jev judge rejected the request with status {status}"
                ))),
            },
            Err(error) => Err(JevHttpError::Fatal(format!(
                "the Jev judge request failed: {error}"
            ))),
        },
        _ = cancel.cancelled() => Err(JevHttpError::Fatal("the Jev judge was cancelled".into())),
    }
}

enum JevHttpError {
    /// 429/529 — back off and retry.
    Retryable(u16),
    /// Everything else — fail at once; the gate escalates.
    Fatal(String),
}

/// Read one exchange's answers and usage into a judgment. Missing or
/// malformed answers fail the call (escalating), never guess.
fn parse_judgment(reply: &serde_json::Value) -> Result<JevJudgment, String> {
    let answers = reply
        .get("answers")
        .ok_or("the Jev judge reply carried no answers")?;
    let noul = |id: &str| -> Result<f64, String> {
        answers
            .get(id)
            .and_then(|answer| answer.get("noul"))
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| format!("the Jev judge reply is missing the `{id}` answer"))
    };
    let parsed = JevAnswers {
        destructive: noul("destructive")?,
        secrets: noul("secrets")?,
        matches_request: noul("matches_request")?,
        needs_human: noul("needs_human")?,
    };
    let usage = reply
        .get("usage")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let input = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Ok(JevJudgment {
        verdict: combine(&parsed),
        input_tokens: input,
        output_tokens: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(destructive: f64, secrets: f64, matches: f64, needs: f64) -> JevAnswers {
        JevAnswers {
            destructive,
            secrets,
            matches_request: matches,
            needs_human: needs,
        }
    }

    #[test]
    fn a_clearly_safe_call_passes() {
        assert_eq!(combine(&answers(0.01, 0.0, 0.99, 0.02)), JevVerdict::Allow);
    }

    #[test]
    fn vetoes_deny_with_their_reason() {
        let JevVerdict::Deny { reason } = combine(&answers(ACTION_THRESHOLD, 0.0, 0.99, 0.0))
        else {
            panic!("destructive at the threshold denies");
        };
        assert!(reason.contains("destructive"), "{reason}");
        let JevVerdict::Deny { reason } = combine(&answers(0.0, 0.9, 0.99, 0.0)) else {
            panic!("secrets denies");
        };
        assert!(reason.contains("credentials"), "{reason}");
    }

    #[test]
    fn a_clearly_unrelated_call_denies() {
        let JevVerdict::Deny { reason } = combine(&answers(0.0, 0.0, DEAD_BAND_LOW, 0.0)) else {
            panic!("unrelated at the lower bound denies");
        };
        assert!(reason.contains("request"), "{reason}");
    }

    #[test]
    fn the_ask_the_user_question_escalates() {
        assert_eq!(
            combine(&answers(0.0, 0.0, 0.99, ACTION_THRESHOLD)),
            JevVerdict::Unsure
        );
    }

    #[test]
    fn dead_band_answers_escalate() {
        for a in [
            answers(0.5, 0.0, 0.99, 0.0),
            answers(0.0, 0.59, 0.99, 0.0),
            answers(0.0, 0.0, 0.5, 0.0),
            answers(0.0, 0.0, 0.99, 0.41),
        ] {
            assert_eq!(combine(&a), JevVerdict::Unsure, "{a:?}");
        }
    }

    #[test]
    fn boundaries_count_as_decisive() {
        // 0.6 is the yes threshold; 0.4 the no bound — neither is dead band.
        assert!(matches!(
            combine(&answers(0.0, ACTION_THRESHOLD, 0.99, 0.0)),
            JevVerdict::Deny { .. }
        ));
        assert!(matches!(
            combine(&answers(0.0, 0.0, ACTION_THRESHOLD, 0.0)),
            JevVerdict::Allow
        ));
        assert!(matches!(
            combine(&answers(0.39, 0.0, 0.99, 0.39)),
            JevVerdict::Allow
        ));
    }
}
