//! Single integration test target for holt-engine: every former `tests/*.rs`
//! file is a module here, so the crate links one test binary instead of ~50.
//! Module names and file names match the original targets one to one.
//!
//! `mcp_config_rpc` and `mcp_http_rpc` stay separate targets: they mutate the
//! process environment, which is not safe while other tests in the same
//! process read it (libgit2, child spawns).

mod common;

mod always_allow_rpc;
mod auto_review_rpc;
mod chat_usage_watch_rpc;
mod compact_command_rpc;
mod compaction_rpc;
mod edit_last_message_rpc;
mod end_to_end_rpc;
mod git_rpc;
mod git_status_rpc;
mod goal_mode_rpc;
mod history_rpc;
mod images_rpc;
mod mcp_ceilings_rpc;
mod mcp_gate_rpc;
mod mcp_lifecycle_rpc;
mod mcp_settings_rpc;
mod mcp_stdio_rpc;
mod message_queue_rpc;
mod overflow_rpc;
mod path_references_rpc;
mod permission_gate_rpc;
mod permission_mode_rpc;
// The whole file gates its fault-injection tests on unix (chmod-based); the
// module gate keeps the helpers from going dead on other platforms.
#[cfg(unix)]
mod persistence_wedge_rpc;
mod plan_mode_rpc;
mod plan_mode_turn_rpc;
mod provider_catalog_rpc;
mod provider_mode_rpc;
mod provider_store_rpc;
mod queue_commands_rpc;
mod read_chat_rpc;
mod resume_rpc;
mod routines_rpc;
mod scripted_provider;
mod search_files_rpc;
mod skills_rpc;
mod subagents_rpc;
mod title_ownership;
mod title_settings;
mod title_task;
mod todo_rpc;
mod turn_change_set_rpc;
mod turn_retry_rpc;
mod turn_terminal_events_rpc;
mod usage_ledger_rpc;
mod usage_meters_rpc;
mod usage_stats_rpc;
mod web_search_rpc;
mod web_tools_rpc;
mod workspace_file_rpc;
mod workspace_files_rpc;
mod worktree_run_rpc;
