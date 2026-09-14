# Native feature parity: the release-blocking matrix

**Issues:** #484 (roadmap N15), #485 (N16), #491 (N22), #492 (N23) ·
**Roadmap:** #469 · **Last updated:** 2026-09-14

This is the single parity matrix issue #492 asks for, and it is enforced:
`zirv verify --builtin`'s `ZCHK-NATIVE-PARITY` check
(`src/commands/workflow/checks/parity.rs`) reads
[`native-runtime-inventory.md`](native-runtime-inventory.md) and this file
together on every run and FAILS when

- an inventory row (a clap verb, or a model-calling call site) has no row
  here,
- a row here names a capability the inventory does not have,
- a row cites a test that does not exist **in the module it names**, a CI
  step that is not a `- name:` step in `.github/workflows/ci.yaml`, or a
  `docs/benchmarks/` file that is not committed,
- a row claims the `live-validated` rung without pointing at a recorded
  evidence file under `docs/benchmarks/`,
- a row that is not `legacy-only` carries no evidence at all and is not named
  under "Release blockers" below, or
- a `legacy-only` row does not say, in its `Requires` cell, why there is no
  native path.

So the matrix cannot quietly claim more than it has, and a new command verb
or a new model-calling call site cannot land without a parity row in the same
change.

**How to cite evidence.** A test is `<module path>::tests::<fn>`: the module
path is resolved to exactly one file under `src/` (`runtime::native::tests::
foo` -> `src/commands/ctx/runtime/native.rs`; `x/mod.rs` and `x.rs` resolve
alike; any suffix of the real path will do, and the check says "ambiguous --
lengthen it" when a short one matches two files), and the `fn` must be
declared in **that** file. A bare name that happens to exist elsewhere in the
tree is not evidence for this row. A CI citation is `CI: <step>`, matched
against whole `- name:` step lines, never against the YAML as a substring. A
recorded run is a repo-relative path under `docs/benchmarks/`.

## Rungs

The `Rung` column is the *strength of the claim*, and nothing in this
document may claim a rung it has not earned:

| Rung | What it means |
|---|---|
| `unit` | an inline `#[cfg(test)]` test in the owning module pins the behaviour deterministically. No process, no network. |
| `integration` | a test drives the real end-to-end path -- fixture provider transport, a real `StateDir`, a real SQLite journal, a real broker, and where relevant a real child process -- not just the pure function. |
| `ci-matrix` | additionally exercised by a named job or step in `.github/workflows/ci.yaml` on ubuntu, macOS and Windows. |
| `live-validated` | a recorded run against a real provider endpoint, with the recording committed under `docs/benchmarks/`. **No row in this document claims this rung.** |
| `legacy-only` | there is no native path, and the `Requires` cell says why. Every one of these is an entitlement limitation, not an unfinished feature -- see the two tables at the end. |

Current distribution, counted by the check itself: 98 `unit`, 35
`integration`, 17 `ci-matrix`, 8 `legacy-only`, 0 `live-validated`, across
158 capabilities (121 command verbs, 37 model-calling call sites).

## Release blockers

One, and it is a blocker for the *rollout decision*, not for the code:

1. **No route has live-provider evidence.** Every row below is fixture- or
   service-level. The `live-validated` rung is unreached for all five direct
   provider transports (Anthropic Messages, OpenAI Responses, Google Gemini,
   chat-completions, Bedrock Converse), because reaching it needs an
   operator's own API auth material and spends that operator's money; a CI
   runner has neither. What must happen to close it is written down in
   [`2026-09-14-native-release-evidence.md`](2026-09-14-native-release-evidence.md)
   ("What an operator must do"), and until it happens the native runtime
   stays opt-in and the default stays the harness.

No capability row is missing evidence. If one ever is, it must be added to
this list by name or the build fails.

## Capability matrix: command verbs

One row per depth-1/depth-2 verb in
[`native-runtime-inventory.md`](native-runtime-inventory.md)'s `## Commands`
table. "Legacy path" is what ran before the native runtime existed; where it
says "same zirv code" there was never a harness dependency to remove.
"Requires" is the provider/platform condition the native path needs -- `none`
means it runs on any supported OS with no provider configured at all.

| Capability | Native path | Legacy path | Requires | Evidence | Rung |
|---|---|---|---|---|---|
| `agent` | `ctx::native_worker::run` | `ctx::agent::run_with` + vendor CLI | any configured native route | `native_worker::tests::only_a_native_completed_status_becomes_a_completed_delegation`, `native_worker::tests::a_native_and_a_legacy_worker_cannot_both_claim_one_task_card` | `integration` |
| `artifact` | `workflow::artifact` | same zirv code | none | `artifact::tests::artifacts_are_referenced_by_id_without_copying_payloads_into_state` | `unit` |
| `artifact list` | `workflow::artifact` | same zirv code | none | `artifact::tests::artifacts_are_referenced_by_id_without_copying_payloads_into_state` | `unit` |
| `artifact present` | `workflow::artifact` + native `artifact_present` tool | same zirv code | none | `artifact::tests::interactive_fallback_obeys_deny_and_requires_approval_for_ask`, `tools::tests::every_capability_tool_parses_through_the_same_closed_registry` | `unit` |
| `artifact render` | `workflow::artifact` | same zirv code | a loopback dev server | `artifact::tests::only_loopback_http_urls_are_dialed_or_opened` | `unit` |
| `artifact show` | `workflow::artifact` | same zirv code | none | `artifact::tests::static_output_is_preferred_without_an_interaction_requirement` | `unit` |
| `chat` | `native::spawn_interactive` under `--runtime native` | `chat::build_launch` + vendor TUI | a real terminal; a route for the `orchestrator` role | `runtime::native::tests::a_native_session_records_its_own_conversation_under_its_own_runtime`, `chat::tests::chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv` | `integration` |
| `commands` | `commands::command_schema` | same zirv code | none | `command_schema::tests::json_output_round_trips_and_is_deterministic` | `unit` |
| `context` | `ctx::context` + `runtime::context::compile` | same zirv code | none | `ctx::context::tests::precedence_ranks_canonical_common_below_harness_specific_below_native` | `unit` |
| `context lint` | `ctx::context_lint` | same zirv code | none | `context_lint::tests::splits_on_terminators_and_line_breaks` | `unit` |
| `context status` | `ctx::context_status` | same zirv code | none | `context_status::tests::mail_addressed_to_a_dead_session_is_swept_and_reported_separately` | `unit` |
| `context sync` | `ctx::context_cli` | same zirv code | none | `context_cli::tests::dispatch_runs_sync_report_end_to_end_in_the_current_directory` | `unit` |
| `create` | `commands::create` | same zirv code | none | `create::tests::a_name_that_leaves_the_zirv_directory_is_rejected` | `unit` |
| `ctx` | `ctx` umbrella | same zirv code | none | `ctx::tests::help_exits_zero_on_every_verb_and_bare_ctx` | `unit` |
| `ctx agent` | `ctx::native_worker::run` | `ctx::agent::run_with` + vendor CLI | any configured native route | `native_worker::tests::the_positional_name_selects_the_route_and_native_defers_to_the_role`, `native_worker::tests::harness_only_arguments_are_refused_rather_than_silently_dropped` | `integration` |
| `ctx api` | `ctx::api::server` over protocol v1 | same zirv code on both runtimes | none | `api::tests::schema_prints_the_human_contract_by_default` | `unit` |
| `ctx ask` | `handoff::helper_answer` at role `ask` -> `ctx::helper` | `ask::run_model` + `distiller_cmd` | a route for the `ask` role | `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path`, `ask::tests::a_live_session_is_asked_and_answers_from_its_own_transcript` | `ci-matrix` |
| `ctx capabilities` | `runtime::capabilities::discover` | same zirv code on both runtimes | none | `capabilities_cmd::tests::an_unconfigured_machine_reports_every_integration_without_failing`, `CI: Native Setup And Doctor With No Harness Installed` | `ci-matrix` |
| `ctx chat` | `native::spawn_interactive` under `--runtime native` | `chat::build_launch` + vendor TUI | a real terminal | `chat::tests::chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv` | `unit` |
| `ctx compile` | `runtime::context::compile` | `ctx::compile` | none | `compile::tests::identical_slices_have_no_prefix_diff` | `unit` |
| `ctx config` | `ctx::config` + `ctx::config_cmd` schema 2 | same zirv code | none | `config_cmd::tests::invalid_edits_leave_file_untouched_including_separate_policy_sections`, `CI: Config Migration Round Trip` | `ci-matrix` |
| `ctx discover` | `ctx::discover` over the compaction ledger | same zirv code | none | `discover::tests::measured_vs_estimated_classification_matches_the_ledger_join_exactly` | `unit` |
| `ctx doctor` | `ctx::doctor::diagnose` | same zirv code on both runtimes | none | `doctor::tests::a_doctor_names_each_failure_class_from_a_real_inventory`, `CI: Verify Native Diagnosis Contracts` | `ci-matrix` |
| `ctx exec` | `native::run_headless` | `exec::run_with_clock_inner` + vendor CLI | any configured native route | `runtime::native::tests::a_whole_native_session_runs_with_an_empty_path_and_no_harness_binary`, `exec::tests::exec_resolves_the_configured_default_against_its_own_role` | `integration` |
| `ctx explain-status` | `ctx::attention` | same zirv code | none | `attention::tests::explain_status_reports_projection_reason_and_skipped_fallbacks` | `unit` |
| `ctx forget` | `ctx::memory` + native `memory_forget` tool | same zirv code | none | `memory::tests::forget_removes_one_key_and_forget_all_empties_the_bank` | `unit` |
| `ctx group` | `ctx::group` + native `group_create`/`group_status` tools | same zirv code | none | `tools::tests::a_native_coordinator_runs_a_mixed_team_through_the_shared_services`, `group::tests::a_work_group_round_trips_through_state_and_lists` | `integration` |
| `ctx handoff` | `handoff::helper_answer` at role `distiller` | `handoff::run_model` + `distiller_cmd` | a route for the `distiller` role | `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path`, `handoff::tests::helper_answer_falls_back_to_the_harness_when_no_native_route_exists` | `ci-matrix` |
| `ctx handover` | -- | `ctx::handover` swaps one vendor CLI for another | a native seat's model is `[roles]` configuration, not a handover -- there is no vendor CLI under a native seat to swap | -- | `legacy-only` |
| `ctx hook` | `ctx::hook` | same zirv code | none | `hook::tests::hook_status_reports_no_baseline_before_any_install` | `unit` |
| `ctx inbox` | `ctx::mail` + native `wait`/`result` tools | same zirv code | none | `mail::tests::inbox_marks_a_fan_out_message_read_without_removing_it_for_other_sessions` | `unit` |
| `ctx kill` | -- | `ctx::sessions::run_kill` terminates a supervised OS process | a native session is interrupted over the runtime protocol; there is no supervised vendor process to signal | -- | `legacy-only` |
| `ctx learn` | `ctx::learn` | same zirv code | none | `learn::tests::classify_error_detects_each_class` | `unit` |
| `ctx loop` | each cycle is a native `ctx exec` | each cycle is a harness `ctx exec` | any configured native route | `run_loop::tests::a_compact_tier_loop_cycle_compacts_and_continues_the_same_session` | `unit` |
| `ctx measure` | `ctx::measure` | same zirv code | none | `measure::tests::median_is_none_for_empty` | `unit` |
| `ctx nudge` | `ctx::delegation::send` queues durably for a native worker | `ctx::sessions::run_nudge` types at an open pane | none | `sessions::tests::a_nudge_stores_a_session_addressed_message_and_a_wake_marker`, `tools::tests::a_message_to_a_worker_with_an_approval_open_is_queued_not_typed` | `unit` |
| `ctx objective` | `ctx::objective` + native `objective_status` tool | same zirv code | none | `tools::tests::operator_steering_and_stopping_reach_the_coordinator`, `objective::tests::a_record_round_trips_through_state` | `integration` |
| `ctx optimize` | `handoff::helper_answer` at role `optimize` | `optimize::run_with` + `distiller_cmd` | a route for the `optimize` role | `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path`, `optimize::tests::every_layer_is_collected_in_a_stable_order` | `ci-matrix` |
| `ctx output` | `runtime::tools` streams into the same store | same zirv code | none | `ctx::output::tests::a_failing_command_is_captured_verbatim_and_summarized` | `unit` |
| `ctx permissions` | `runtime::enforcement` consumes the same policy | same zirv code | none | `enforcement::tests::approval_is_bound_to_action_policy_generation_and_parent_identity`, `permissions::tests::classify_approval_categorizes_each_exclusion_reason` | `integration` |
| `ctx provider` | `provider::inventory` + `provider::credential` | not applicable -- native-only surface | none | `provider_cmd::tests::credential_set_refuses_harness_login_stores_before_reading_or_writing`, `CI: Native Setup And Doctor With No Harness Installed` | `ci-matrix` |
| `ctx recall` | `ctx::memory` + native `memory_recall` tool | same zirv code | none | `memory::tests::ctx_recall_merges_both_banks_and_labels_each_entrys_provenance` | `unit` |
| `ctx remember` | `ctx::memory` + native `memory_remember` tool | same zirv code | none | `memory::tests::remembering_an_existing_key_replaces_the_entry_rather_than_duplicating_it` | `unit` |
| `ctx resume` | `native::resume_journal` | `resume::launch_command` + vendor CLI | any configured native route | `runtime::native::tests::a_resume_reconciles_a_started_execution_and_fences_the_old_generation`, `resume::tests::an_unknown_adapter_preserves_the_crash_witness` | `integration` |
| `ctx run` | `runtime::tools` reuses the same store and permits | same zirv code | none | `ctx::output::tests::the_summary_respects_the_byte_cap_and_still_carries_the_retrieval_line` | `unit` |
| `ctx safety` | `runtime::enforcement` re-evaluates the same classifier | same zirv code | none | `safety::tests::evaluate_candidate_outcome_matches_the_existing_fold_for_a_single_candidate` | `unit` |
| `ctx savings` | `ctx::ledger` | same zirv code | none | `ledger::tests::savings_on_an_empty_ledger_prints_a_no_rows_line_and_exits_zero` | `unit` |
| `ctx score` | journal projection into the same scoring vocabulary | harness transcript parser | none | `runtime::native::tests::the_journal_projection_scores_deterministically_through_the_pure_engine`, `score::tests::breakdown_for_session_computes_a_real_summary_for_a_registered_session` | `integration` |
| `ctx search` | `ctx::search` + native `context_search` tool | same zirv code | none | `search::tests::demoted_sessions_collects_loop_and_exec_verbs_only` | `unit` |
| `ctx send` | `ctx::delegation::send` + native `send` tool | same zirv code | none | `mail::tests::a_mail_body_cannot_readdress_itself`, `ctx::delegation::tests::a_message_blocked_by_an_open_approval_is_queued_then_delivered_once` | `integration` |
| `ctx snapshot` | `ctx::snapshot` with `redact_text` | same zirv code | none | `snapshot::tests::cap_head_tail_is_a_no_op_under_budget`, `doctor::tests::a_diagnostic_dump_carries_no_secret_transcript_or_continuation_data` | `unit` |
| `ctx spend` | `ctx::spend` over the same ledger; native calls reconcile into the route pool | same zirv code | none | `spend::tests::spend_by_harness_over_the_fixture_ledger_matches_hand_computed_totals`, `runtime::native::tests::every_provider_request_reconciles_once_into_this_routes_pool` | `integration` |
| `ctx status` | `ctx::status` reads the native journal | same zirv code | none | `status::tests::the_native_recovery_section_is_absent_without_a_native_journal` | `unit` |
| `ctx swarm` | `ctx::task` cards claimed by either runtime | same zirv code | none | `task::tests::swarm_writes_every_card_in_one_atomic_batch`, `tools::tests::two_workers_can_never_claim_one_card` | `integration` |
| `ctx task` | `ctx::task` + native `task_create`/`task_claim`/`task_list` tools | same zirv code | none | `tools::tests::two_workers_can_never_claim_one_card`, `task::tests::claim_refuses_a_card_that_is_not_ready` | `integration` |
| `ctx usage` | `ctx::usage`; native usage arrives from the journal | same zirv code | none | `usage::tests::tee_persists_the_windows_and_chains_the_original_command` | `unit` |
| `ctx wait` | `ctx::attention` + native `wait` tool | same zirv code | none | `attention::tests::wait_times_out_when_the_projection_never_matches` | `unit` |
| `ctx worktree` | `ctx::worktree` | same zirv code | none | `worktree::tests::ahead_count_parses_a_plain_integer_with_trailing_newline` | `unit` |
| `ctx wrap` | -- | `wrap::run_with` supervises a vendor TUI over a PTY | a native session has no vendor TUI process to wrap; the native interactive surface is `zirv chat --runtime native` | -- | `legacy-only` |
| `frontend` | `workflow::frontend` | same zirv code | none | `frontend::tests::profile_bootstraps_without_operator_input_and_reuses_current_cache` | `unit` |
| `frontend benchmark` | `workflow::frontend_detector` | same zirv code | none | `frontend_detector::tests::benchmark_corpus_has_no_detector_drift` | `unit` |
| `frontend capabilities` | `workflow::capability` | same zirv code | none | `workflow::capability::tests::a_report_distinguishes_available_unavailable_and_unverified_integrations` | `unit` |
| `frontend check` | `workflow::frontend_detector` | same zirv code | none | `frontend_detector::tests::objective_accessibility_hazards_are_blocking` | `unit` |
| `frontend profile` | `workflow::frontend` | same zirv code | none | `frontend::tests::profile_refreshes_when_repository_evidence_changes` | `unit` |
| `frontend render` | `workflow::frontend_render` + a configured browser capability | same zirv code | a configured browser integration (not a coding harness) | `frontend_render::tests::server_discovery_is_bounded_and_uses_argv_not_a_shell`, `workflow::capability::tests::workflow_admission_refuses_an_unavailable_integration_and_names_the_missing_piece` | `unit` |
| `frontend review` | `review::reviewer_argv(Native, ..)` for the visual reviewer | `launch_visual_reviewer` + vendor CLI | a route for the reviewer role; a configured browser capability for the render half | `review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_flags` | `unit` |
| `help` | `commands::help` | same zirv code | none | `help::tests::help_always_shows_usage_and_the_builtins` | `unit` |
| `init` | `commands::init` | same zirv code | none | `init::tests::init_is_non_interactive_when_answers_are_supplied` | `unit` |
| `memory` | `ctx::memory_cli` | same zirv code | none | `memory_cli::tests::parses_every_verb` | `unit` |
| `memory forget` | `ctx::memory` + native `memory_forget` tool | same zirv code | none | `memory_cli::tests::forget_and_verify_work_in_all_three_scopes_even_when_disabled` | `unit` |
| `memory init` | `ctx::memory_cli` | same zirv code | none | `memory_cli::tests::parses_every_verb` | `unit` |
| `memory list` | `ctx::memory_cli` | same zirv code | none | `memory_cli::tests::list_defaults_to_private_and_shared_needs_the_flag` | `unit` |
| `memory optimize` | `memory_optimize::apply_consolidation` -> `helper_answer` at role `distiller` | `apply_consolidation` -> `distiller_cmd` | a route for the `distiller` role | `memory_optimize::tests::exact_duplicates_are_flagged_with_key_and_path_evidence`, `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `memory promote` | `ctx::memory_cli` | same zirv code | none | `memory_cli::tests::parses_every_verb` | `unit` |
| `memory recall` | `ctx::memory` + native `memory_recall` tool | same zirv code | none | `memory_cli::tests::shared_list_output_carries_an_untrusted_content_note` | `unit` |
| `memory remember` | `ctx::memory` + native `memory_remember` tool | same zirv code | none | `memory::tests::remember_global_writes_under_the_global_slug_and_never_under_the_repo_slug` | `unit` |
| `memory rollback` | `ctx::memory` | same zirv code | none | `memory::tests::rollback_of_an_overwrite_restores_the_prior_body` | `unit` |
| `memory status` | `ctx::memory_cli` | same zirv code | none | `memory_cli::tests::status_counts_entries_and_bytes_per_scope_without_printing_bodies` | `unit` |
| `memory verify` | `ctx::memory_cli` | same zirv code | none | `memory_cli::tests::verify_reports_an_error_and_nonzero_when_the_key_is_absent` | `unit` |
| `report` | `commands::report` | same zirv code | none | `report::tests::body_sources_are_mutually_exclusive` | `unit` |
| `report bug` | `commands::report` | same zirv code | none | `report::tests::bug_report_appends_environment_and_uses_bug_label` | `unit` |
| `report feature` | `commands::report` | same zirv code | none | `report::tests::feature_report_reads_body_file_and_uses_enhancement_label` | `unit` |
| `session` | `ctx::session::native` hosts a native conversation | `ctx::session::host` hosts a PTY | none | `session::tests::every_verb_refuses_while_the_gate_is_off` | `unit` |
| `session attach` | `ctx::session::native` | `ctx::session::host` | none | `session::native::tests::detaching_every_client_leaves_the_conversation_and_its_registry_record`, `session::host::tests::many_observers_may_watch_but_only_one_client_holds_the_keyboard` | `integration` |
| `session detach` | `ctx::session::native` | `ctx::session::host` | none | `session::host::tests::detaching_leaves_the_process_and_its_screen_alive_for_the_next_client` | `integration` |
| `session list` | `ctx::session::service` over protocol v1 | same zirv code on both runtimes | none | `session::service::tests::a_served_runtime_serves_the_attachment_surface_over_the_real_transport` | `integration` |
| `session serve` | `ctx::session::service` | same zirv code on both runtimes | none | `session::service::tests::a_live_runtime_is_refused_and_a_recycled_pid_is_replaceable`, `session::service::tests::an_unverifiable_record_is_refused_rather_than_taken_over` | `integration` |
| `session stop` | `ctx::session::native` ends the conversation | `ctx::session::host` ends the process | none | `session::native::tests::an_interrupt_cancels_the_turn_and_a_stop_ends_the_conversation`, `session::host::tests::stopping_ends_the_process_and_releases_its_registry_record` | `integration` |
| `setup` | `commands::setup` (harness hook installer); the native path is `ctx provider init` + `ctx doctor` | same zirv code | none | `setup::tests::parses_status_apply_and_guarded_reset`, `CI: Native Setup And Doctor With No Harness Installed` | `ci-matrix` |
| `setup apply` | `commands::setup` | same zirv code | none | `setup::tests::install_claude_integration_records_a_baseline_on_a_reapply_with_no_changes` | `unit` |
| `setup profile` | `commands::setup` | same zirv code | none | `setup::tests::profile_dry_run_previews_then_wet_run_merges_backs_up_and_is_idempotent` | `unit` |
| `setup reset` | `commands::setup` | same zirv code | none | `setup::tests::reset_refuses_a_symlink_anywhere_in_a_target_tree_before_deleting_anything` | `unit` |
| `setup restore` | `commands::setup` | same zirv code | none | `setup::tests::reset_backs_up_exact_targets_and_preserves_auth_by_default` | `unit` |
| `setup status` | `commands::setup` | same zirv code | none | `setup::tests::status_json_has_a_stable_versioned_shape` | `unit` |
| `skill` | `workflow::skill` + `runtime::context::compile` | same zirv code | none | `workflow::skill::tests::builtins_are_valid_compact_and_provider_neutral` | `unit` |
| `skill list` | `workflow::skill` | same zirv code | none | `workflow::skill::tests::stable_id_and_version_resolution_is_identical_across_adapters` | `unit` |
| `skill show` | `workflow::skill` | same zirv code | none | `workflow::skill::tests::an_operator_global_skill_still_overrides_a_built_in` | `unit` |
| `test` | `workflow::verification` | same zirv code | none | `verification::tests::discovers_rust_checks_without_external_services` | `unit` |
| `test all` | `workflow::verification` | same zirv code | none | `verification::tests::disabled_repo_checks_are_listed_but_never_executed` | `unit` |
| `test baseline` | `workflow::verification` | same zirv code | none | `verification::tests::baseline_round_trips_through_the_operator_home_directory`, `verification::tests::a_failure_not_in_the_baseline_blocks_the_gate_and_names_the_new_failure` | `unit` |
| `test changed` | `workflow::verification` | same zirv code | none | `verification::tests::changed_paths_since_base_includes_untracked_and_drops_deleted_paths` | `unit` |
| `update` | `commands::update` | same zirv code | none | `update::tests::assets_match_supported_platforms_and_release_urls` | `unit` |
| `verify` | `workflow::verification` + `workflow::checks` | same zirv code | none | `verification::tests::verify_still_runs_checks_the_test_gate_did_not_cover`, `parity::tests::the_real_repo_parity_matrix_passes`, `CI: Verify Native Parity Matrix` | `ci-matrix` |
| `version` | `commands::version` | same zirv code | none | `version::tests::test_get_version_output` | `unit` |
| `workflow` | `workflow::engine` | same zirv code | none | `commands::workflow::tests::top_level_workflow_command_is_case_insensitive` | `unit` |
| `workflow advance` | `engine::advance_with_evidence` + native `workflow_advance` tool | same zirv code | none | `tools::tests::a_session_with_no_writer_permit_can_read_a_workflow_but_never_advance_it`, `engine::tests::advance_run_checks_runs_the_test_gate_and_advances_on_success` | `integration` |
| `workflow agents` | `agents::dispatch_native_seat` (read-only seats) | `agents::dispatch_agent` + vendor CLI | a route for the `seat` role | `agents::tests::a_writable_seat_is_refused_by_the_native_dispatcher`, `agents::tests::an_unknown_dispatch_runtime_is_refused` | `unit` |
| `workflow approve` | `engine::approve` + native `workflow_approve` tool | same zirv code | none | `engine::tests::approval_gate_must_be_explicitly_released` | `unit` |
| `workflow artifacts` | `workflow::artifact` + native `artifact_register`/`artifact_present` tools | same zirv code | none | `engine::tests::workflow_artifact_status_reports_pending_accepted_and_drifted` | `unit` |
| `workflow classify` | `workflow::classify` | same zirv code | none | `classify::tests::identical_inputs_produce_identical_classification` | `unit` |
| `workflow close` | `workflow::engine` | same zirv code | none | `engine::tests::close_succeeds_after_residual_dispositions_and_clears_active` | `unit` |
| `workflow context` | `engine::render_current_context` + native `workflow_context` tool | same zirv code | none | `engine::tests::workflow_context_over_the_configured_cap_is_truncated_with_a_visible_marker` | `unit` |
| `workflow list` | `workflow::engine` | same zirv code | none | `engine::tests::load_reports_a_domain_error_for_an_unknown_workflow_id` | `unit` |
| `workflow maintain` | `workflow::maintain` | same zirv code | none | `maintain::tests::detector_validation_is_bounded` | `unit` |
| `workflow reclassify` | `workflow::engine` | same zirv code | none | `engine::tests::reclassify_preserves_completed_steps_and_accepted_artifacts` | `unit` |
| `workflow resume` | `workflow::engine` | same zirv code | none | `engine::tests::resume_does_not_redispatch_completed_steps` | `unit` |
| `workflow review` | `review::reviewer_argv(Native, ..)` -> `zirv agent --runtime native --mode read-only` | `launch_reviewer` + vendor CLI | a route for the reviewer role | `review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_flags`, `review::tests::a_finding_recorded_while_the_reviewer_ran_survives_the_evidence_write` | `integration` |
| `workflow show` | `workflow::engine` | same zirv code | none | `engine::tests::write_state_renders_completed_step_wall_clock_only_when_known` | `unit` |
| `workflow start` | `workflow::engine` | same zirv code | none | `engine::tests::start_profile_flag_overrides_automatic_classification` | `unit` |
| `workflow stats` | `workflow::telemetry` | same zirv code | none | `telemetry::tests::stats_report_json_carries_the_overall_cache_hit_ratio_field` | `unit` |
| `workflow status` | `workflow::engine` + native `workflow_status` tool | same zirv code | none | `tools::tests::a_session_with_no_writer_permit_can_read_a_workflow_but_never_advance_it` | `integration` |

## Capability matrix: model-calling call sites

One row per entry in the inventory's `## Model-calling entry points` table,
keyed `path::symbol` exactly as that table spells them.

| Capability | Native path | Legacy path | Requires | Evidence | Rung |
|---|---|---|---|---|---|
| `src/commands/ctx/provider/anthropic.rs::perform_blocking` | direct HTTPS/SSE Anthropic Messages transport | the `claude` CLI | an Anthropic API key; any OS | `anthropic::tests::stream_reassembles_thinking_signatures_usage_and_multiple_tools`, `anthropic::tests::http_retry_after_and_midstream_overload_are_typed` | `integration` |
| `src/commands/ctx/provider/openai.rs::perform_blocking` | direct HTTPS/SSE OpenAI Responses transport | the `codex` CLI | an OpenAI API key; any OS | `openai::tests::stream_reassembles_reasoning_text_and_multiple_function_calls`, `openai::tests::http_retry_after_is_carried_on_rate_limits` | `integration` |
| `src/commands/ctx/provider/google.rs::perform_blocking` | direct HTTPS/SSE Gemini transport (Developer and Vertex profiles) | the `gemini` CLI | a Google API key or Vertex auth material; any OS | `google::tests::stream_reassembles_thought_signatures_usage_and_parallel_function_calls`, `google::tests::quota_and_generic_errors_classify_with_retry_hints` | `integration` |
| `src/commands/ctx/provider/openai_chat.rs::perform_blocking` | direct HTTPS/SSE chat-completions transport for every compatible vendor, local runtime and Azure OpenAI | the vendor's own CLI, where one exists | a route profile and its auth material; any OS | `openai_chat::tests::the_stream_reassembles_text_and_every_streamed_tool_call`, `openai_chat::tests::a_length_stop_drops_every_tool_call_and_records_the_omission` | `integration` |
| `src/commands/ctx/provider/bedrock.rs::perform_blocking` | SigV4-signed Bedrock Converse transport, AWS event-stream decoded in-tree | no legacy path -- Bedrock had none | AWS auth material and a region; any OS | `bedrock::tests::the_event_stream_reassembles_text_tool_use_and_signed_reasoning`, `bedrock::tests::a_max_tokens_stop_drops_every_tool_call_and_records_it` | `integration` |
| `src/commands/ctx/runtime/native.rs::stream_once` | the native agent loop's single provider call site | the vendor CLI's own agent loop | any configured native route | `runtime::native::tests::two_disconnects_are_retried_within_the_response_budget`, `runtime::native::tests::a_continuation_request_carries_the_retrys_result_not_the_failed_attempt` | `integration` |
| `src/commands/ctx/runtime/compaction.rs::distill` | compaction distillation through the session's own `ProviderAdapter` | the harness's own compaction | any configured native route | `runtime::native::tests::a_long_session_compacts_on_token_pressure_and_keeps_its_objective_constraints_and_history`, `compaction::tests::a_provider_overflow_forces_a_compaction_at_any_token_count` | `integration` |
| `src/commands/ctx/runtime/native.rs::run_headless` | `zirv ctx exec --runtime native` | `exec::run_with_clock_inner` + vendor CLI | any configured native route | `runtime::native::tests::a_whole_native_session_runs_with_an_empty_path_and_no_harness_binary`, `runtime::native::tests::the_final_status_serializes_with_its_schema_version_and_actual_route` | `integration` |
| `src/commands/ctx/runtime/native.rs::spawn_interactive` | `zirv chat --runtime native` drives a `NativeLoop` per submitted turn | a vendor TUI in a dashboard pane | a real terminal; any configured native route | `runtime::native::tests::a_native_session_records_its_own_conversation_under_its_own_runtime`, `runtime::native::tests::a_second_submit_while_a_turn_is_running_is_busy_not_a_silent_interleave` | `integration` |
| `src/commands/ctx/chat.rs::build_launch` | resolved through `runtime::resolve` to the native backend | builds the vendor TUI argv | none | `chat::tests::chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv` | `unit` |
| `src/commands/ctx/wrap.rs::run_with` | -- | first-launch PTY spawn of a vendor TUI | a native session has no vendor TUI to supervise | -- | `legacy-only` |
| `src/commands/ctx/wrap.rs::relaunch` | -- | in-place PTY restart after compaction/handoff | same as `wrap::run_with`: there is no vendor TUI under a native seat | -- | `legacy-only` |
| `src/commands/ctx/dash/roster.rs::restore_argv` | rebuilds a native pane's resume argv | rebuilds a harness pane's resume argv | none | `dash::pane::tests::a_resume_relaunch_carries_the_role_layer_and_no_handoff_prompt` | `unit` |
| `src/commands/ctx/dash/pane.rs::spawn` | a native pane hosts `spawn_interactive` | a pane hosts a vendor TUI | none | `runtime::native::tests::a_stale_or_uncommitted_generation_may_not_open_a_native_pane` | `unit` |
| `src/commands/ctx/dash/pane.rs::handover` | -- | swaps the vendor CLI under a live pane | a native seat changes model through `[roles]`/`--route`, not by swapping a vendor CLI | -- | `legacy-only` |
| `src/commands/ctx/dash/mod.rs::fulfill_spawn_request` | fulfils a native worker/pane spawn request | fulfils a harness one | none | `dash::tests::a_depth_refusal_is_not_retryable` | `unit` |
| `src/commands/ctx/exec.rs::run_with_clock_inner` | forks to `native::run_headless` before any adapter is selected | `supervise::spawn_tapped` on a vendor CLI | none | `exec::tests::exec_resolves_the_configured_default_against_its_own_role` | `unit` |
| `src/commands/ctx/exec.rs::compact_in_place` | `runtime::compaction` inside the same native session | resumes the vendor CLI in place | none | `runtime::native::tests::a_context_overflow_recovers_through_a_compaction_without_repeating_an_effect` | `integration` |
| `src/commands/ctx/run_loop.rs::run_with_clock` | each cycle is a native `ctx exec` | each cycle spawns the vendor CLI | none | `run_loop::tests::a_compact_tier_loop_cycle_compacts_and_continues_the_same_session` | `unit` |
| `src/commands/ctx/run_loop.rs::evaluate_objective_after_cycle` | `handoff::helper_answer` at role `distiller` | `handoff::run_model` + `distiller_cmd` | a route for the `distiller` role | `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `src/commands/ctx/handoff.rs::helper_answer` | the one chokepoint every non-chat model call reaches: native first | falls back to `run_model` when the role has no native route | a route for the helper's role | `handoff::tests::helper_answer_falls_back_to_the_harness_when_no_native_route_exists`, `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `src/commands/ctx/handoff.rs::run_model` | -- | the harness half behind `helper_answer`; wraps `distiller_cmd` | kept deliberately as the fallback when a role has no native route -- not a gap | `handoff::tests::run_model_gives_up_at_the_timeout` | `legacy-only` |
| `src/commands/ctx/helper.rs::run` | one bounded, read-only native session per helper call | no legacy equivalent -- native-only | a route for the helper's role | `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path`, `helper::tests::a_helper_that_tries_to_write_is_refused_by_the_broker` | `ci-matrix` |
| `src/commands/workflow/agents.rs::dispatch_native_seat` | read-only built-in seats through `ctx::helper` | `dispatch_agent` + vendor CLI | a route for the `seat` role | `agents::tests::a_writable_seat_is_refused_by_the_native_dispatcher` | `unit` |
| `src/commands/ctx/resume.rs::launch_command` | `native::resume_journal` reconciles and fences before the session may run | relaunches the vendor CLI with its own resume flag | none | `runtime::native::tests::a_resume_reconciles_a_started_execution_and_fences_the_old_generation` | `integration` |
| `src/commands/ctx/agent.rs::run_with` | forks to `native_worker::run` after the shared claim/permit/reservation | probes `headless_resume_cmd` and spawns the vendor CLI | none | `native_worker::tests::a_native_worker_cannot_take_a_checkout_a_legacy_worker_already_holds` | `integration` |
| `src/commands/ctx/native_worker.rs::run` | `zirv agent --runtime native`: shared ownership, durable receipt, `native::run_session` | no legacy equivalent -- native-only | any configured native route | `native_worker::tests::only_a_native_completed_status_becomes_a_completed_delegation`, `native_worker::tests::a_native_and_a_legacy_worker_cannot_both_claim_one_task_card` | `integration` |
| `src/commands/ctx/delegation.rs::AgentLauncher` | the native `delegate` tool's launcher -- one `agent::run_with` call | same zirv code | none | `ctx::delegation::tests::a_launch_receipt_is_durable_before_anything_runs`, `ctx::delegation::tests::a_crash_between_persistence_and_delivery_republishes_the_same_outcome` | `integration` |
| `src/commands/workflow/agents.rs::dispatch_agent` | -- | synchronous `.status()` dispatch of a built-in seat onto a vendor CLI | the harness half of `workflow agents dispatch`; the native half is `dispatch_native_seat` | `agents::tests::builtins_are_provider_neutral_and_read_only_seats_cannot_require_writes` | `legacy-only` |
| `src/commands/workflow/review.rs::launch_reviewer` | emits `zirv agent --runtime native --mode read-only` with no adapter flags | emits a vendor-CLI reviewer argv | a route for the reviewer role | `review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_flags` | `unit` |
| `src/commands/workflow/frontend_render.rs::launch_visual_reviewer` | reuses `review::reviewer_argv` with `--runtime native` | same builder, harness runtime | a route for the reviewer role; a configured browser capability | `review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_flags` | `unit` |
| `src/commands/workflow/engine.rs::spawn_auto_worker` | re-execs `zirv workflow review run`/`test`/`verify`, which resolve their own runtime | same zirv code | none | `engine::tests::auto_spawn_decision_truth_table` | `unit` |
| `src/commands/ctx/ask.rs::run_model` | `handoff::helper_answer` at role `ask` | `distiller_cmd` child | a route for the `ask` role | `ask::tests::asking_never_touches_the_transcript_the_registry_record_or_leaves_a_nudge_or_mail`, `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `src/commands/ctx/optimize.rs::run_with` | `handoff::helper_answer` at role `optimize` | `distiller_cmd` child | a route for the `optimize` role | `optimize::tests::settings_surfaces_survive_even_when_nested_instruction_files_exceed_the_cap`, `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `src/commands/ctx/memory.rs::harvest_durable_with_tool_errors` | `handoff::helper_answer` at role `distiller` | `distiller_cmd` child | a route for the `distiller` role | `memory::tests::a_body_bullet_cannot_rewrite_the_header`, `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `src/commands/ctx/memory_optimize.rs::apply_consolidation` | `handoff::helper_answer` at role `distiller` | `distiller_cmd` child | a route for the `distiller` role | `memory_optimize::tests::a_lexical_contradiction_is_flagged_with_evidence`, `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` | `ci-matrix` |
| `src/commands/workflow/checks/argv.rs::headless_cmd` | builds argv to inspect it; never spawns | same zirv code | none | `argv::tests::claude_headless_passes_against_the_real_adapter` | `unit` |

## The four invariants, and what pins each

Issue #492's fault-injection criterion names four things that must never
happen. They are asserted by name, so a reader can go from the invariant to
the test without reading the whole suite.

| Invariant | Faults injected | Tests |
|---|---|---|
| No lost acknowledged input | stream drop mid-turn; rollover in every direction; queued mail across a rollover | `runtime::native::tests::an_acknowledged_input_reaches_the_conversation_exactly_once`, `runtime::native::tests::two_disconnects_are_retried_within_the_response_budget`, `runtime::native::tests::a_disconnect_past_the_retry_budget_fails_explicitly`, `rollover_runtime::tests::every_direction_and_trigger_commits_without_losing_acknowledged_state`, `rollover_runtime::tests::queued_mail_survives_every_rollover_direction_and_is_delivered_exactly_once` |
| No duplicated exclusive work | two runtimes racing one task card; a coordinator restart; a republished outcome; queued mail across a rollover | `tools::tests::two_workers_can_never_claim_one_card`, `native_worker::tests::a_native_and_a_legacy_worker_cannot_both_claim_one_task_card`, `coordinator::tests::a_restarted_coordinator_consumes_pending_receipts_exactly_once`, `runtime::native::tests::a_coordinator_session_resumes_its_graph_and_settles_a_node_exactly_once`, `ctx::delegation::tests::a_terminal_outcome_publishes_once_and_replays_as_a_duplicate`, `rollover_runtime::tests::queued_mail_survives_every_rollover_direction_and_is_delivered_exactly_once` |
| No blind repeated external mutation | tool failure mid-call; provider 5xx/timeout mid-turn; a crash between intent and effect; the approval channel closing while a call is blocked; a refused journal commit | `runtime::native::tests::a_safe_tool_failure_is_retried_and_a_reconcile_tool_never_is`, `runtime::native::tests::an_outcome_unknown_effect_forces_an_incomplete_final_status`, `runtime::native::tests::a_continuation_request_carries_the_retrys_result_not_the_failed_attempt`, `journal::tests::execution_intent_precedes_effect_and_crash_recovery_never_retries_blindly`, `enforcement::tests::a_prompt_channel_dropped_while_a_call_is_blocked_grants_nothing`, `runtime::native::tests::a_fenced_generation_commits_nothing_and_runs_no_effect` |
| No two write-capable seat generations | a resume against a live generation; a rollover mid-preparation; a refused journal commit at a stale generation | `journal::tests::stale_generation_cannot_append_or_advance`, `runtime::native::tests::a_stale_or_uncommitted_generation_may_not_open_a_native_pane`, `runtime::native::tests::a_resume_reconciles_a_started_execution_and_fences_the_old_generation`, `runtime::native::tests::a_fenced_generation_commits_nothing_and_runs_no_effect`, `rollover_runtime::tests::every_direction_and_trigger_commits_without_losing_acknowledged_state` |

Faults covered elsewhere, with the test that owns each: **rate exhaustion**
-- `pace::tests::an_exhausted_window_waits_past_the_reset_then_proceeds`,
`pace::tests::classified_provider_rate_limits_reach_the_existing_limit_path`,
`health::tests::a_burst_of_rate_limits_cannot_make_a_replay_reopen_a_healed_route`;
**provider/proxy 5xx and overload** --
`anthropic::tests::http_retry_after_and_midstream_overload_are_typed`,
`openai::tests::http_retry_after_is_carried_on_rate_limits`,
`google::tests::quota_and_generic_errors_classify_with_retry_hints`;
**provider timeout** --
`provider::transport::tests::a_worker_that_never_speaks_trips_the_first_event_deadline`,
`anthropic::tests::first_event_idle_and_in_flight_cancellation_are_enforced`;
**service crash and takeover** --
`session::service::tests::a_live_runtime_is_refused_and_a_recycled_pid_is_replaceable`,
`session::service::tests::an_unverifiable_record_is_refused_rather_than_taken_over`;
**every rollover direction** --
`rollover_runtime::tests::every_runtime_pair_has_a_named_direction_and_unknown_is_an_error`
enumerates native->native, native->wrapped, wrapped->native and
wrapped->wrapped, and
`every_direction_and_trigger_commits_without_losing_acknowledged_state` drives
all four with every trigger.

The mixed-runtime proof and the four invariants above are not Linux-only:
the `Verify Mixed Runtime And Fault Invariants` step of the `Native Install`
job runs the seat-fence, approval-channel, queued-mail and rollover tests on
ubuntu, macOS and Windows, and
`runtime::tests::a_mixed_board_exchanges_mail_and_survives_a_return_to_the_harness_default`
drives a wrapped and a native seat on one board through mail in both
directions and then a return to the harness default with every seat record,
conversation reference and unread message intact.

## Entitlement limitations versus implementation gaps

Added by N22 (#491), unchanged by N23. The `legacy-only` rows above mix
nothing: every one of them is an entitlement limitation. `zirv ctx doctor`
makes the same separation at runtime -- `upstream-entitlement` versus
`missing-tool` -- and this is the authoritative list behind that
classification. No entry is a bare "later": an implementation gap names its
owner, and an entitlement limitation names why it is not ours.

### Genuine upstream entitlement limitations

| Limitation | Why it is upstream, not ours | Where it surfaces |
|---|---|---|
| A Claude.ai / ChatGPT subscription is not an API entitlement | the plan is sold for that vendor's own CLI; there is no direct-API grant to spend | `doctor` class `upstream-entitlement`; the route stops at `configured` with the problem named (`inventory::tests::subscription_route_stops_at_configured_without_harming_api_route`) |
| Harness login tokens are refused as native auth material | reusing the vendor CLI's stored login for direct API calls is outside what that token is issued for | `credential::refuse_harness_login`, before a secret is read (`provider_cmd::tests::credential_set_refuses_harness_login_stores_before_reading_or_writing`) |
| `zirv ctx wrap` | supervises a vendor TUI by definition; a native session has no PTY to wrap | `legacy-only` rows above |
| `zirv ctx handover` | swaps one vendor CLI for another; a native seat's model is `[roles]` configuration, not a handover | `legacy-only` rows above |
| `zirv ctx kill` | terminates a supervised OS process; a native session is interrupted over the runtime protocol | `legacy-only` rows above |
| A model absent from an account's own model list | that account's entitlement with the vendor | `doctor` class `inaccessible-model`, from the live model-list probe |

### Implementation gaps (ours, each tracked)

| Gap | Owner | How it is reported |
|---|---|---|
| A route profile with no native adapter yet | the tracking issue named in the profile's own `Support::Planned` | `doctor` class `missing-tool`; the message says "this is a zirv gap, not an upstream entitlement limit" verbatim (`provider::inventory`) |
| Windows process isolation (restricted token / AppContainer helper) | N04 (#473); no verified backend shipped | `doctor` class `unsupported-isolation`; `enforcement` refuses a sandboxed invocation rather than running it unconfined |
| Harness-transcript rot parsing for native sessions | N09 (#478) | the native journal projects into the same scoring vocabulary; a *harness* transcript parser stays adapter-specific |
| Writable built-in agent seats on the native seat dispatcher | by design, #484 -- a writable seat is a delegated worker with a real permit | `agents::tests::a_writable_seat_is_refused_by_the_native_dispatcher` |
| Live-provider validation of every row in this document | N19 (#488) | the `live-validated` rung is unreachable; no row here claims it, and it is the one release blocker above |
