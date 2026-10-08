//! Single binary for this crate's integration tests.
//!
//! Every file in this directory is a module of this one test binary: one
//! link per rebuild instead of one per file. Cargo treats `tests/suite/main.rs`
//! as the single `suite` target and never builds the sibling files on their own,
//! so a new file must be declared here (`scripts/check-test-suites.*` enforces it).

mod autoscope_env;
mod backfill_dry_run;
mod backfill_e2e;
mod backfill_failures;
mod backfill_member;
mod backup_agents;
mod completions;
mod doctor_e2e;
mod e2e_support;
mod external_capture;
mod external_capture_powershell;
mod external_capture_ts;
mod hook_drain;
mod hook_payload;
mod jail_toggles_e2e;
mod marker_scope;
mod message_e2e;
mod opencode_dialect;
mod packaging;
mod profile_e2e;
mod removal;
mod repo_layout;
mod routing_instructions;
mod routing_skills;
mod run_offline;
mod serve_shutdown;
mod server_profiles;
mod shutdown_signals;
mod upgrade_e2e;
mod workstream_provenance;
mod yolo_ai_jail;

mod workstream_identity;
