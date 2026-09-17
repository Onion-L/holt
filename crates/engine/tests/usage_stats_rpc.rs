//! Handle-seam tests for the device-level `UsageStats` aggregate
//! (usage-overview spec, ticket 01). The engine is assembled over a
//! hand-built data directory — usage ledgers, the device archive, and the
//! chats archive are plain fixture files — and every assertion goes
//! through `RpcService::handle("UsageStats")`, exactly like the UI reads
//! it: all five record kinds count, the archive merges in (deleted chats
//! stay counted), ranges filter and zero-fill on local-timezone days,
//! (provider, model) keys stay distinct and sort by total, cwd attribution
//! keeps full paths apart and parks the unresolvable under "Deleted
//! chats", the cache-hit denominator excludes cache writes, and the
//! heatmap is a fixed 365 days whatever the range says.

use std::path::Path;

use chrono::{Local, MappedLocalTime, NaiveDate, TimeZone};
use holt_engine::{EngineConfig, LocalEngine};
use holt_rpc::{RpcReply, RpcService, methods};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures: plain JSONL ledgers, the archive, and the chats archive
// ---------------------------------------------------------------------------

/// One usage-record JSON line — the wire shape a ledger file carries.
/// Token fields distinct per record so a mis-attribution stands out.
#[allow(clippy::too_many_arguments)]
fn record_line(
    kind: &str,
    provider: &str,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    timestamp_millis: i64,
) -> String {
    format!(
        r#"{{"kind":"{kind}","provider":"{provider}","model":"{model}","input":{input},"output":{output},"cacheRead":{cache_read},"cacheWrite":{cache_write},"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":{timestamp_millis}}}"#
    )
}

/// An archive line is a record restamped with its chat id.
fn archive_line(
    chat_id: &str,
    kind: &str,
    provider: &str,
    model: &str,
    input: u64,
    output: u64,
    timestamp_millis: i64,
) -> String {
    format!(
        r#"{{"kind":"{kind}","provider":"{provider}","model":"{model}","chatId":"{chat_id}","input":{input},"output":{output},"cacheRead":0,"cacheWrite":0,"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":{timestamp_millis}}}"#
    )
}

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// A per-chat ledger: version header first, one record per line.
fn write_ledger(data_dir: &Path, chat_id: &str, lines: &[String]) {
    let mut text = String::from("{\"version\":1}\n");
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    write_file(
        &data_dir.join("usage").join(format!("{chat_id}.jsonl")),
        &text,
    );
}

fn write_archive(data_dir: &Path, lines: &[String]) {
    let mut text = String::from("{\"version\":1}\n");
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    write_file(&data_dir.join("usage").join("archive.jsonl"), &text);
}

/// One chat row — the fields `Chat` requires, cwd the interesting one.
fn chat_row(id: &str, cwd: Value) -> Value {
    json!({
        "id": id,
        "deviceId": "dev",
        "title": null,
        "archived": false,
        "cwd": cwd,
        "branch": null,
        "checkoutId": null,
        "config": null,
        "lastMessagePreview": null,
        "lastMessageAt": null,
        "createdAt": "2026-01-01T00:00:00Z"
    })
}

fn write_chats(data_dir: &Path, rows: Vec<Value>) {
    write_file(
        &data_dir.join("chats.json"),
        &serde_json::to_string(&rows).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// Local-time stamps
// ---------------------------------------------------------------------------

fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// The epoch-millis stamp of a local wall-clock time, robust to DST
/// ambiguities (earliest wins) and gaps (step forward an hour).
fn local_stamp(date: NaiveDate, hour: u32, minute: u32) -> i64 {
    let naive = date.and_hms_opt(hour, minute, 0).unwrap();
    let stamp = match Local.from_local_datetime(&naive) {
        MappedLocalTime::Single(stamp) => stamp,
        MappedLocalTime::Ambiguous(earliest, _) => earliest,
        MappedLocalTime::None => {
            let stepped = naive
                .checked_add_signed(chrono::Duration::hours(1))
                .unwrap();
            Local.from_local_datetime(&stepped).single().unwrap()
        }
    };
    stamp.timestamp_millis()
}

fn days_ago_at(days: u32, hour: u32, minute: u32) -> i64 {
    local_stamp(today() - chrono::Duration::days(days as i64), hour, minute)
}

// ---------------------------------------------------------------------------
// The RPC seam
// ---------------------------------------------------------------------------

fn engine(data_dir: &Path) -> LocalEngine {
    LocalEngine::assemble(&EngineConfig {
        data_dir: data_dir.to_path_buf(),
        personal_skills_dir: None,
        stream_fn: None,
        search_backend_resolver: None,
    })
    .unwrap()
}

async fn usage_stats(engine: &LocalEngine, days: u32) -> Value {
    let RpcReply::Value(reply) = engine
        .handle(methods::USAGE_STATS, json!({ "days": days }))
        .await
        .unwrap()
    else {
        panic!("UsageStats did not return a value");
    };
    reply
}

/// The model series' date keys (oldest first), asserted uniform across models.
fn series_dates(reply: &Value) -> Vec<String> {
    let empty: Vec<Value> = Vec::new();
    let first = reply["models"]
        .as_array()
        .unwrap()
        .first()
        .map(|series| series["days"].as_array().unwrap())
        .unwrap_or(&empty);
    first
        .iter()
        .map(|day| day["date"].as_str().unwrap().to_string())
        .collect()
}

fn day_tokens(reply: &Value, model: &str, date: &str) -> u64 {
    let full_id = model.to_string();
    let series = reply["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|series| {
            format!(
                "{}/{}",
                series["provider"].as_str().unwrap(),
                series["model"].as_str().unwrap()
            ) == full_id
        })
        .unwrap_or_else(|| panic!("no series for {model}: {}", reply["models"]));
    series["days"]
        .as_array()
        .unwrap()
        .iter()
        .find(|day| day["date"] == date)
        .unwrap_or_else(|| panic!("no {date} bucket for {model}"))["tokens"]
        .as_u64()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_five_kinds_count_and_the_archive_merges_deleted_chats_in() {
    let dir = TempDir::new().unwrap();
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[
            record_line(
                "turn",
                "openai",
                "gpt-5.4",
                100,
                10,
                3,
                4,
                days_ago_at(0, 12, 0),
            ),
            record_line(
                "subagent",
                "openai",
                "gpt-5.4",
                50,
                5,
                0,
                0,
                days_ago_at(0, 13, 0),
            ),
            record_line(
                "compaction",
                "openai",
                "gpt-5.4",
                20,
                2,
                0,
                0,
                days_ago_at(1, 9, 0),
            ),
            record_line(
                "auto-review",
                "anthropic",
                "claude-opus",
                30,
                3,
                0,
                0,
                days_ago_at(0, 10, 0),
            ),
            record_line(
                "title",
                "anthropic",
                "claude-opus",
                10,
                1,
                0,
                0,
                days_ago_at(0, 10, 5),
            ),
        ],
    );
    write_ledger(
        dir.path(),
        "chat-live-2",
        &[record_line(
            "turn",
            "openai",
            "gpt-5.4",
            7,
            7,
            0,
            0,
            days_ago_at(0, 14, 0),
        )],
    );
    // A deleted chat survives only as archive rows — still counted.
    write_archive(
        dir.path(),
        &[
            archive_line(
                "chat-gone",
                "turn",
                "anthropic",
                "claude-opus",
                500,
                50,
                days_ago_at(0, 8, 0),
            ),
            archive_line(
                "chat-gone",
                "title",
                "openai",
                "gpt-5.4",
                5,
                5,
                days_ago_at(0, 8, 5),
            ),
        ],
    );
    write_chats(
        dir.path(),
        vec![
            chat_row("chat-live-1", json!("/tmp/proj-a")),
            chat_row("chat-live-2", json!("/tmp/proj-a")),
        ],
    );

    let reply = usage_stats(&engine(dir.path()), 30).await;

    // The reply decodes through the tolerant frame the UI reads.
    let typed: holt_proto::UsageStatsReply = serde_json::from_value(reply.clone()).unwrap();
    assert_eq!(typed.days, 30);

    // Header: three chats with in-range records — two live by file name,
    // one deleted by its restamped archive id.
    assert_eq!(reply["chatCount"], 3, "{reply}");

    let totals = &reply["totals"];
    assert_eq!(totals["input"], 100 + 50 + 20 + 30 + 10 + 7 + 500 + 5);
    assert_eq!(totals["output"], 10 + 5 + 2 + 3 + 1 + 7 + 50 + 5);
    assert_eq!(totals["cacheRead"], 3);
    assert_eq!(totals["cacheWrite"], 4);
    // Today and yesterday both carry records.
    assert_eq!(totals["activeDays"], 2);

    // Two distinct (provider, model) keys, sorted by total descending:
    // anthropic 594 > openai 218.
    let models = reply["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["provider"], "anthropic");
    assert_eq!(models[0]["model"], "claude-opus");
    assert_eq!(models[1]["provider"], "openai");
    assert_eq!(models[1]["model"], "gpt-5.4");
    assert_eq!(models[0]["days"].as_array().unwrap().len(), 30);

    let by_model = reply["byModel"].as_array().unwrap();
    assert_eq!(by_model[0]["provider"], "anthropic");
    assert_eq!(by_model[0]["total"], 30 + 3 + 10 + 1 + 500 + 50);
    assert_eq!(
        by_model[1]["total"],
        100 + 10 + 3 + 4 + 50 + 5 + 20 + 2 + 7 + 7 + 5 + 5
    );
    // The breakdown's columns: no cache-write column beyond the totals.
    assert_eq!(by_model[1]["input"], 182);
    assert_eq!(by_model[1]["cacheRead"], 3);

    // Projects: the two live chats share /tmp/proj-a; the deleted chat has
    // no resolvable directory, so its group sorts first by total (560).
    let by_project = reply["byProject"].as_array().unwrap();
    assert_eq!(by_project.len(), 2);
    assert_eq!(by_project[0]["path"], Value::Null);
    assert_eq!(by_project[0]["total"], 500 + 50 + 5 + 5);
    let gone_rows = by_project[0]["chats"].as_array().unwrap();
    assert_eq!(gone_rows.len(), 1);
    assert_eq!(gone_rows[0]["chatId"], "chat-gone");
    assert_eq!(gone_rows[0]["total"], 560);
    assert_eq!(by_project[1]["path"], "/tmp/proj-a");
    assert_eq!(by_project[1]["total"], 117 + 55 + 22 + 33 + 11 + 14);
    assert_eq!(by_project[1]["chats"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn the_range_filters_zero_fills_and_the_heatmap_stays_year_long() {
    let dir = TempDir::new().unwrap();
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[
            // In range: today.
            record_line(
                "turn",
                "openai",
                "gpt-5.4",
                100,
                10,
                0,
                0,
                days_ago_at(0, 12, 0),
            ),
            // Exactly `days` back is outside a 7-day range (today-6 ..= today).
            record_line(
                "turn",
                "openai",
                "gpt-5.4",
                800,
                80,
                0,
                0,
                days_ago_at(7, 12, 0),
            ),
            // Far outside the range but inside the heatmap year.
            record_line(
                "turn",
                "anthropic",
                "claude-opus",
                50,
                5,
                0,
                0,
                days_ago_at(200, 12, 0),
            ),
            // Older than even the heatmap.
            record_line(
                "turn",
                "anthropic",
                "claude-opus",
                900,
                90,
                0,
                0,
                days_ago_at(400, 12, 0),
            ),
        ],
    );
    write_chats(
        dir.path(),
        vec![chat_row("chat-live-1", json!("/tmp/proj-a"))],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;
    assert_eq!(reply["days"], 7);

    // Only today's record is inside the range.
    assert_eq!(reply["chatCount"], 1);
    assert_eq!(reply["totals"]["input"], 100);
    assert_eq!(reply["totals"]["output"], 10);
    assert_eq!(reply["totals"]["activeDays"], 1);

    // Only the in-range model gets a series: the anthropic records are all
    // outside the range, so no all-zero entry is invented for them.
    let models = reply["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["provider"], "openai");
    for series in models {
        assert_eq!(series["days"].as_array().unwrap().len(), 7, "{series}");
    }
    let dates = series_dates(&reply);
    assert_eq!(dates.len(), 7);
    assert_eq!(
        dates.last().unwrap().as_str(),
        today().format("%Y-%m-%d").to_string()
    );
    assert_eq!(
        dates.first().unwrap().as_str(),
        (today() - chrono::Duration::days(6))
            .format("%Y-%m-%d")
            .to_string()
    );
    assert_eq!(
        day_tokens(&reply, "openai/gpt-5.4", dates.last().unwrap()),
        110
    );
    // Yesterday's bucket exists, zero-filled.
    assert_eq!(day_tokens(&reply, "openai/gpt-5.4", &dates[5]), 0);

    // The heatmap ignores the range: 365 fixed buckets ending today, the
    // 200-day-old record in its bucket, the 400-day-old one gone.
    let heatmap = reply["heatmap"].as_array().unwrap();
    assert_eq!(heatmap.len(), 365);
    assert_eq!(
        heatmap.first().unwrap()["date"],
        (today() - chrono::Duration::days(364))
            .format("%Y-%m-%d")
            .to_string()
    );
    assert_eq!(
        heatmap.last().unwrap()["date"],
        today().format("%Y-%m-%d").to_string()
    );
    let old_key = (today() - chrono::Duration::days(200))
        .format("%Y-%m-%d")
        .to_string();
    let old_bucket = heatmap
        .iter()
        .find(|day| day["date"] == old_key.as_str())
        .unwrap();
    assert_eq!(old_bucket["tokens"], 55);
    let heatmap_sum: u64 = heatmap
        .iter()
        .map(|day| day["tokens"].as_u64().unwrap())
        .sum();
    // Every record within the year counts — today's (110), exactly-7-days
    // ago (880), 200 days ago (55) — the 400-day-old one excluded.
    assert_eq!(heatmap_sum, 110 + 880 + 55);

    // A wider range re-filters (the 7-day-back record joins; the 200-day
    // one is still out) but the heatmap is identical data.
    let wide = usage_stats(&engine(dir.path()), 90).await;
    assert_eq!(wide["totals"]["input"], 100 + 800);
    let wide_heatmap = wide["heatmap"].as_array().unwrap();
    assert_eq!(wide_heatmap.len(), 365);
    assert_eq!(
        serde_json::to_value(wide_heatmap).unwrap(),
        serde_json::to_value(heatmap).unwrap(),
        "the heatmap must not move with the range"
    );
}

#[tokio::test]
async fn day_buckets_follow_the_local_midnight_boundary() {
    let dir = TempDir::new().unwrap();
    let midnight = local_stamp(today(), 0, 0);
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[
            // One millisecond before local midnight belongs to yesterday;
            // midnight itself opens today.
            record_line("turn", "openai", "gpt-5.4", 10, 0, 0, 0, midnight - 1),
            record_line("turn", "openai", "gpt-5.4", 20, 0, 0, 0, midnight),
        ],
    );
    write_chats(
        dir.path(),
        vec![chat_row("chat-live-1", json!("/tmp/proj-a"))],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;
    let dates = series_dates(&reply);
    let yesterday = &dates[dates.len() - 2];
    let today_key = &dates[dates.len() - 1];
    assert_eq!(day_tokens(&reply, "openai/gpt-5.4", yesterday), 10);
    assert_eq!(day_tokens(&reply, "openai/gpt-5.4", today_key), 20);
    // Two distinct local days are two active days.
    assert_eq!(reply["totals"]["activeDays"], 2);
}

#[tokio::test]
async fn same_model_name_under_two_providers_stays_two_entries() {
    let dir = TempDir::new().unwrap();
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[
            record_line(
                "turn",
                "provider-a",
                "gpt-x",
                100,
                0,
                0,
                0,
                days_ago_at(0, 12, 0),
            ),
            record_line(
                "turn",
                "provider-b",
                "gpt-x",
                300,
                0,
                0,
                0,
                days_ago_at(0, 12, 5),
            ),
        ],
    );
    write_chats(
        dir.path(),
        vec![chat_row("chat-live-1", json!("/tmp/proj-a"))],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;

    let by_model = reply["byModel"].as_array().unwrap();
    assert_eq!(by_model.len(), 2, "same model, different providers");
    assert_eq!(by_model[0]["provider"], "provider-b");
    assert_eq!(by_model[0]["model"], "gpt-x");
    assert_eq!(by_model[0]["total"], 300);
    assert_eq!(by_model[1]["provider"], "provider-a");
    assert_eq!(by_model[1]["total"], 100);
    // The series carry the same order.
    let models = reply["models"].as_array().unwrap();
    assert_eq!(models[0]["provider"], "provider-b");
    assert_eq!(models[1]["provider"], "provider-a");
}

#[tokio::test]
async fn projects_key_on_full_paths_and_unresolvable_chats_group_together() {
    let dir = TempDir::new().unwrap();
    write_ledger(
        dir.path(),
        "chat-a",
        &[record_line(
            "turn",
            "openai",
            "gpt-5.4",
            100,
            0,
            0,
            0,
            days_ago_at(0, 12, 0),
        )],
    );
    write_ledger(
        dir.path(),
        "chat-b",
        &[record_line(
            "turn",
            "openai",
            "gpt-5.4",
            200,
            0,
            0,
            0,
            days_ago_at(0, 12, 5),
        )],
    );
    // A live chat whose row carries no cwd parks in the deleted group too.
    write_ledger(
        dir.path(),
        "chat-c",
        &[record_line(
            "turn",
            "openai",
            "gpt-5.4",
            30,
            0,
            0,
            0,
            days_ago_at(0, 13, 0),
        )],
    );
    write_archive(
        dir.path(),
        &[archive_line(
            "chat-gone",
            "turn",
            "openai",
            "gpt-5.4",
            400,
            0,
            days_ago_at(0, 8, 0),
        )],
    );
    // Same basename, different directories: never merged.
    write_chats(
        dir.path(),
        vec![
            chat_row("chat-a", json!("/work/api")),
            chat_row("chat-b", json!("/other/api")),
            chat_row("chat-c", Value::Null),
        ],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;

    let by_project = reply["byProject"].as_array().unwrap();
    assert_eq!(by_project.len(), 3, "{by_project:?}");
    // Total descending: the deleted group 430, then the two projects.
    assert_eq!(by_project[0]["path"], Value::Null);
    assert_eq!(by_project[0]["total"], 430);
    assert_eq!(by_project[1]["path"], "/other/api");
    assert_eq!(by_project[1]["total"], 200);
    assert_eq!(by_project[2]["path"], "/work/api");
    assert_eq!(by_project[2]["total"], 100);
    // The deleted group expands per chat id, chat id ascending.
    let gone = by_project[0]["chats"].as_array().unwrap();
    assert_eq!(gone.len(), 2);
    assert_eq!(gone[0]["chatId"], "chat-c");
    assert_eq!(gone[0]["total"], 30);
    assert_eq!(gone[1]["chatId"], "chat-gone");
    assert_eq!(gone[1]["total"], 400);
}

#[tokio::test]
async fn cache_hit_excludes_cache_writes_from_the_denominator() {
    let dir = TempDir::new().unwrap();
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[
            // 30 cache reads over 70 prompt inputs → 0.3; the 500 written
            // tokens must not dilute the rate.
            record_line(
                "turn",
                "openai",
                "gpt-5.4",
                70,
                0,
                30,
                500,
                days_ago_at(0, 12, 0),
            ),
            // A range with no prompt tokens at all has no rate.
            record_line(
                "turn",
                "openai",
                "gpt-5.4",
                0,
                0,
                0,
                10,
                days_ago_at(1, 12, 0),
            ),
        ],
    );
    write_chats(
        dir.path(),
        vec![chat_row("chat-live-1", json!("/tmp/proj-a"))],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;
    let hit = reply["totals"]["cacheHit"].as_f64().unwrap();
    assert!((hit - 0.3).abs() < 1e-9, "{hit}");
    assert_eq!(reply["totals"]["cacheWrite"], 510);
}

#[tokio::test]
async fn a_range_with_no_prompt_tokens_reports_no_cache_hit() {
    let dir = TempDir::new().unwrap();
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[record_line(
            "turn",
            "openai",
            "gpt-5.4",
            0,
            50,
            0,
            10,
            days_ago_at(0, 12, 0),
        )],
    );
    write_chats(
        dir.path(),
        vec![chat_row("chat-live-1", json!("/tmp/proj-a"))],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;
    assert!(reply["totals"]["cacheHit"].is_null(), "{reply}");
    assert_eq!(reply["totals"]["cacheWrite"], 10);
}

#[tokio::test]
async fn a_damaged_ledger_is_skipped_read_only_never_quarantined() {
    let dir = TempDir::new().unwrap();
    write_file(
        &dir.path().join("usage").join("chat-bad.jsonl"),
        "garbage, not a header\n",
    );
    write_ledger(
        dir.path(),
        "chat-live-1",
        &[record_line(
            "turn",
            "openai",
            "gpt-5.4",
            100,
            0,
            0,
            0,
            days_ago_at(0, 12, 0),
        )],
    );
    write_chats(
        dir.path(),
        vec![chat_row("chat-live-1", json!("/tmp/proj-a"))],
    );

    let reply = usage_stats(&engine(dir.path()), 7).await;

    // The damaged file's records are gone but nothing else is disturbed:
    // no quarantine rename, no error to the caller.
    assert_eq!(reply["chatCount"], 1);
    assert_eq!(reply["totals"]["input"], 100);
    assert!(dir.path().join("usage").join("chat-bad.jsonl").exists());
    let quarantined: Vec<_> = std::fs::read_dir(dir.path().join("usage"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".corrupt"))
        .collect();
    assert!(quarantined.is_empty(), "{quarantined:?}");
}

#[tokio::test]
async fn a_range_offered_list_is_enforced() {
    let dir = TempDir::new().unwrap();
    let error = match engine(dir.path())
        .handle(methods::USAGE_STATS, json!({ "days": 13 }))
        .await
    {
        Ok(_) => panic!("days=13 must be refused"),
        Err(error) => error,
    };
    let message = match error {
        holt_rpc::RpcError::BadParams(message) => message,
        other => panic!("expected BadParams, got {other}"),
    };
    assert!(message.contains("7, 30, or 90"), "{message}");
}
