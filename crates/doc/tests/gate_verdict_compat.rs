//! Gate verdict wire compatibility (ADR-0026): review verdicts gained a
//! `judge` field; records from before it exists must keep decoding as the
//! chat model's verdicts, and the new field must round-trip.

use holt_doc::{GateVerdict, ReviewJudge};
use serde_json::json;

#[test]
fn review_verdicts_round_trip_with_their_judge() {
    for verdict in [
        GateVerdict::ReviewPassed {
            judge: ReviewJudge::ChatModel,
        },
        GateVerdict::ReviewPassed {
            judge: ReviewJudge::Jev,
        },
        GateVerdict::ReviewRejected {
            reason: Some("use pnpm".into()),
            judge: ReviewJudge::Jev,
        },
        GateVerdict::ReviewRejected {
            reason: None,
            judge: ReviewJudge::ChatModel,
        },
    ] {
        let encoded = serde_json::to_value(&verdict).unwrap();
        assert_eq!(
            serde_json::from_value::<GateVerdict>(encoded).unwrap(),
            verdict
        );
    }
}

#[test]
fn pre_judge_records_decode_as_the_chat_model() {
    // A verdict from before the field existed: no judge, no reason.
    assert_eq!(
        serde_json::from_value::<GateVerdict>(json!({"kind": "reviewPassed"})).unwrap(),
        GateVerdict::ReviewPassed {
            judge: ReviewJudge::ChatModel
        }
    );
    assert_eq!(
        serde_json::from_value::<GateVerdict>(json!({
            "kind": "reviewRejected",
            "reason": "no tests",
        }))
        .unwrap(),
        GateVerdict::ReviewRejected {
            reason: Some("no tests".into()),
            judge: ReviewJudge::ChatModel,
        }
    );
}
