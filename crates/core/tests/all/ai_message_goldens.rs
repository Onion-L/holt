//! Serialization-parity tests against TypeScript oracle goldens produced by
//! `scripts/oracle/generate-message-goldens.mts` (see tests/goldens/ai/).
//!
//! The goldens are `JSON.stringify` output of objects shaped by
//! `pi-core/ai/src/types.ts`. Each test parses the golden into the Rust type,
//! re-serializes, and compares the parsed structure — and round-trips the Rust
//! value through JSON to check lossless deserialization.

use pi_core::ai::types::{
    AssistantMessage, AssistantMessageEvent, Context, DeferredHandle, Message, Model, Tool, Usage,
    UserContent, UserMessage,
};

fn load_golden(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/tests/goldens/ai/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read golden {name}: {error}"));
    serde_json::from_str(&raw).expect("golden parses as JSON")
}

fn assert_round_trip<T>(name: &str)
where
    T: serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug + PartialEq,
{
    let golden = load_golden(name);
    let parsed: T = serde_json::from_value(golden.clone())
        .unwrap_or_else(|error| panic!("failed to parse golden {name} into Rust type: {error}"));
    let output = serde_json::to_value(&parsed)
        .unwrap_or_else(|error| panic!("failed to serialize {name}: {error}"));
    assert_eq!(
        output, golden,
        "re-serialized {name} differs from the TS oracle golden"
    );
    let raw = serde_json::to_string(&output).expect("JSON string");
    let reparsed: T = serde_json::from_str(&raw).expect("round-trip parse");
    assert_eq!(reparsed, parsed, "round-trip changed {name}");
}

#[test]
fn assistant_message_matches_oracle() {
    assert_round_trip::<AssistantMessage>("assistantMessage");
}

#[test]
fn assistant_minimal_matches_oracle() {
    assert_round_trip::<AssistantMessage>("assistantMinimal");
}

#[test]
fn assistant_deferred_matches_oracle() {
    assert_round_trip::<AssistantMessage>("assistantDeferred");
}

#[test]
fn message_with_error_matches_oracle() {
    assert_round_trip::<AssistantMessage>("messageWithError");
}

#[test]
fn user_string_matches_oracle() {
    assert_round_trip::<UserMessage>("userString");
}

#[test]
fn user_blocks_matches_oracle() {
    assert_round_trip::<UserMessage>("userBlocks");
}

#[test]
fn tool_result_matches_oracle() {
    assert_round_trip::<pi_core::ai::types::ToolResultMessage>("toolResult");
}

#[test]
fn tool_result_error_matches_oracle() {
    assert_round_trip::<pi_core::ai::types::ToolResultMessage>("toolResultError");
}

#[test]
fn deferred_handle_matches_oracle() {
    assert_round_trip::<DeferredHandle>("deferredHandle");
}

#[test]
fn usage_matches_oracle() {
    assert_round_trip::<Usage>("usage");
}

#[test]
fn messages_match_oracle() {
    assert_round_trip::<Vec<Message>>("messages");
}

#[test]
fn events_match_oracle() {
    assert_round_trip::<Vec<AssistantMessageEvent>>("events");
}

#[test]
fn model_matches_oracle() {
    assert_round_trip::<Model>("model");
}

#[test]
fn model_compat_samples_match_oracle() {
    let golden = load_golden("modelCompatSamples");
    let obj = golden.as_object().expect("compat samples object");
    for (name, value) in obj {
        let compat: pi_core::ai::types::ModelCompat =
            serde_json::from_value(value.clone()).expect("compat parses");
        let output = serde_json::to_value(&compat).expect("compat serializes");
        assert_eq!(
            &output, value,
            "compat sample {name} differs from the TS oracle golden"
        );
    }
}

#[test]
fn tool_matches_oracle() {
    assert_round_trip::<Tool>("tool");
}

#[test]
fn context_matches_oracle() {
    assert_round_trip::<Context>("context");
}

#[test]
fn user_content_round_trips() {
    let golden = load_golden("userString");
    let message: UserMessage = serde_json::from_value(golden).expect("parses");
    assert!(matches!(message.content, UserContent::Text(_)));
}
