//! The device-level usage aggregate behind `UsageStats` (usage-overview
//! spec, ticket 01): one unary reply merging every surviving Usage record
//! on the device — live chats' ledgers and the grow-only archive, all five
//! source kinds — into the ranged metrics, per-model daily series, and
//! breakdowns the Usage overview renders. Strictly read-only: a damaged
//! ledger is skipped, never quarantined, and the chats archive is the only
//! attribution file consulted — viewing stats never touches a running
//! chat's files.
//!
//! Day buckets are local-timezone calendar days, one convention shared by
//! the range series, Active days, and the heatmap. The range metrics and
//! both breakdowns cover only the selected window; the heatmap is always
//! the fixed 365 days ending today.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, Local, NaiveDate};

use crate::store::load_chats;
use crate::usage::{self, UsageRecord};

/// The heatmap's fixed window — 365 local days ending today, independent
/// of the request's range.
const HEATMAP_DAYS: i64 = 365;

/// A record's timestamp (epoch millis) as its local calendar day.
fn local_date(timestamp_millis: i64) -> Option<NaiveDate> {
    DateTime::from_timestamp_millis(timestamp_millis)
        .map(|moment| moment.with_timezone(&Local).date_naive())
}

/// The reply's day-key spelling: the local calendar day, `YYYY-MM-DD`.
fn day_key(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// The four headline token fields — what every total, breakdown row, and
/// day bucket sums. A record's `cacheWrite1h` and `reasoning` are real
/// ledger fields but not part of holt's gross total, so they stay out.
#[derive(Clone, Copy, Debug, Default)]
struct Tokens {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Tokens {
    fn of(record: &UsageRecord) -> Self {
        Self {
            input: record.input,
            output: record.output,
            cache_read: record.cache_read,
            cache_write: record.cache_write,
        }
    }

    fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    /// Total = the four fields summed — the ledger's gross definition.
    fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// One (provider, model) key's range sums and its gross tokens per day.
#[derive(Default)]
struct ModelAcc {
    tokens: Tokens,
    per_day: BTreeMap<NaiveDate, u64>,
}

/// One breakdown group's range sums. The per-chat rows matter only to the
/// "Deleted chats" group; project rows carry the group totals alone.
#[derive(Default)]
struct GroupAcc {
    tokens: Tokens,
    chats: BTreeMap<String, Tokens>,
}

pub(crate) fn stats(data_dir: &Path, days: u32) -> holt_proto::UsageStatsReply {
    let today = Local::now().date_naive();
    let range_start = today - Duration::days(days as i64 - 1);
    let heat_start = today - Duration::days(HEATMAP_DAYS - 1);

    // The chats archive is the only working-directory source: project =
    // the chat row's cwd, and a chat it does not cover (archive rows of
    // deleted chats, cwd-less rows) falls into the "Deleted chats" group.
    let cwds: BTreeMap<String, Option<String>> = match load_chats(data_dir) {
        Ok(chats) => chats.into_iter().map(|chat| (chat.id, chat.cwd)).collect(),
        Err(error) => {
            tracing::warn!(target: "holt::usage_stats", %error, "could not read the chats archive; every record falls into the Deleted chats group");
            BTreeMap::new()
        }
    };

    // Live ledgers first — chat attribution is the file name — then the
    // archive, whose rows carry their restamped chat id.
    let mut ledgers: Vec<(String, Vec<UsageRecord>)> = Vec::new();
    for chat_id in usage::ledger_chat_ids(data_dir) {
        match usage::load_records(data_dir, &chat_id) {
            Ok(records) => ledgers.push((chat_id, records)),
            Err(reason) => {
                // The runtime quarantines the file when the chat next
                // opens; the aggregate only skips it.
                tracing::warn!(target: "holt::usage_stats", %reason, "skipping a damaged usage ledger");
            }
        }
    }
    let archive = usage::load_archive_records(data_dir);

    let mut chats_in_range = BTreeSet::new();
    let mut active_days = BTreeSet::new();
    let mut totals = Tokens::default();
    let mut models: BTreeMap<(String, String), ModelAcc> = BTreeMap::new();
    let mut projects: BTreeMap<Option<String>, GroupAcc> = BTreeMap::new();
    let mut heatmap: BTreeMap<NaiveDate, u64> = BTreeMap::new();

    let mut account = |chat_id: &str, record: &UsageRecord| {
        let Some(date) = local_date(record.timestamp) else {
            return;
        };
        // A future stamp (clock skew) belongs to no window this reply
        // serves — the range and the heatmap both end today.
        if date > today {
            return;
        }
        let gross = record.gross();
        if date >= heat_start {
            *heatmap.entry(date).or_default() += gross;
        }
        if date < range_start {
            return;
        }
        chats_in_range.insert(chat_id.to_string());
        active_days.insert(date);
        let tokens = Tokens::of(record);
        totals.add(&tokens);
        let model = models
            .entry((record.provider.clone(), record.model.clone()))
            .or_default();
        model.tokens.add(&tokens);
        *model.per_day.entry(date).or_default() += gross;
        let group = projects
            .entry(cwds.get(chat_id).cloned().flatten())
            .or_default();
        group.tokens.add(&tokens);
        group
            .chats
            .entry(chat_id.to_string())
            .or_default()
            .add(&tokens);
    };
    for (chat_id, records) in &ledgers {
        for record in records {
            account(chat_id, record);
        }
    }
    for record in &archive {
        account(record.chat_id.as_deref().unwrap_or(""), record);
    }

    // Day buckets are zero-filled oldest-first: the range series covers
    // exactly `days`, the heatmap exactly 365, whatever the records do.
    let filled_days =
        |per_day: &BTreeMap<NaiveDate, u64>, start: NaiveDate| -> Vec<holt_proto::UsageStatsDay> {
            let mut series = Vec::new();
            let mut cursor = start;
            while cursor <= today {
                series.push(holt_proto::UsageStatsDay {
                    date: day_key(cursor),
                    tokens: per_day.get(&cursor).copied().unwrap_or(0),
                });
                cursor += Duration::days(1);
            }
            series
        };

    // BTreeMap iteration is (provider, model) ascending; the stable sort
    // by total keeps that order inside ties.
    let mut model_list: Vec<((String, String), ModelAcc)> = models.into_iter().collect();
    model_list.sort_by_key(|((provider, model), acc)| {
        (
            std::cmp::Reverse(acc.tokens.total()),
            provider.clone(),
            model.clone(),
        )
    });
    let model_rows: Vec<(&str, &str, Tokens, Vec<holt_proto::UsageStatsDay>)> = model_list
        .iter()
        .map(|((provider, model), acc)| {
            (
                provider.as_str(),
                model.as_str(),
                acc.tokens,
                filled_days(&acc.per_day, range_start),
            )
        })
        .collect();

    let model_series = model_rows
        .iter()
        .map(|(provider, model, _, days)| holt_proto::UsageModelSeries {
            provider: (*provider).to_string(),
            model: (*model).to_string(),
            days: days.clone(),
        })
        .collect();
    let by_model = model_rows
        .iter()
        .map(
            |(provider, model, tokens, _)| holt_proto::UsageModelBreakdown {
                provider: (*provider).to_string(),
                model: (*model).to_string(),
                input: tokens.input,
                output: tokens.output,
                cache_read: tokens.cache_read,
                total: tokens.total(),
            },
        )
        .collect();

    // BTreeMap<Option<String>, _> iterates None (the Deleted chats group)
    // before the paths; the stable total sort keeps that inside ties.
    let mut by_project: Vec<holt_proto::UsageProjectGroup> = projects
        .into_iter()
        .map(|(path, group)| {
            // Per-chat rows are the Deleted chats group's alone.
            let chats = if path.is_none() {
                group
                    .chats
                    .into_iter()
                    .map(|(chat_id, tokens)| holt_proto::UsageChatBreakdown {
                        chat_id,
                        input: tokens.input,
                        output: tokens.output,
                        cache_read: tokens.cache_read,
                        total: tokens.total(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            holt_proto::UsageProjectGroup {
                path,
                input: group.tokens.input,
                output: group.tokens.output,
                cache_read: group.tokens.cache_read,
                total: group.tokens.total(),
                chats,
            }
        })
        .collect();
    by_project.sort_by_key(|group| std::cmp::Reverse(group.total));

    // Cache hit = cache reads over prompt tokens (input + cache reads);
    // cache writes stay out of the denominator, and a range with no
    // prompt tokens at all has no rate.
    let prompt_denominator = totals.input + totals.cache_read;

    holt_proto::UsageStatsReply {
        chat_count: chats_in_range.len() as u64,
        days,
        totals: holt_proto::UsageStatsTotals {
            input: totals.input,
            output: totals.output,
            cache_read: totals.cache_read,
            cache_write: totals.cache_write,
            cache_hit: (prompt_denominator > 0)
                .then(|| totals.cache_read as f64 / prompt_denominator as f64),
            active_days: active_days.len() as u32,
        },
        models: model_series,
        by_model,
        by_project,
        heatmap: filled_days(&heatmap, heat_start),
    }
}
