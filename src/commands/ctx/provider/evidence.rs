//! Redacted live-contract evidence manifest for the native provider routes
//! (issue #592).
//!
//! Every production route's transport is proven against replayed fixtures,
//! never against a real vendor endpoint -- see `docs/design/2026-09-14-
//! native-release-evidence.md` §3.1. Closing that gap needs an operator's
//! own API key and spends that operator's money, so nothing in this
//! repository can collect the evidence itself. What this module gives an
//! operator instead is the tooling that records it correctly once they can:
//! the ignored `live_*` contract tests in `anthropic.rs`, `openai.rs`,
//! `google.rs` and `openai_chat.rs` are the ONLY callers of
//! [`record_stream_result`]; they require real credentials to run at all,
//! and they do nothing in ordinary CI (`#[ignore]`, never selected by a
//! bare `cargo test`/`cargo nextest`).
//!
//! `record_stream_result` never receives a credential directly -- callers
//! have none to pass, by construction -- but a failure body can still echo
//! one back verbatim (a self-hosted OpenAI-compatible proxy putting the raw
//! bearer token in its JSON `error.message`, say), so every free-text field
//! goes through two redaction passes before it is written: first the
//! calling adapter's own [`super::adapter::ProviderAdapter::redact_failure`],
//! which knows the exact credential it authenticated with and blanks any
//! literal occurrence of it, then the generic
//! [`crate::commands::ctx::pace::redact_for_log`] heuristic, which catches
//! common secret *shapes* the exact pass cannot know about. `record_stream_
//! result` takes the adapter as a required argument specifically so this
//! ordering cannot be skipped by a future caller -- there is no lower-level
//! entry point that writes to the manifest without it.
//!
//! The manifest states its own collection status on every row precisely so
//! it can never be mistaken for evidence it does not contain: a fresh
//! checkout's [`template`] has every row `NotCollected` with no model, no
//! outcome and no detail, and only a real live run -- which only an
//! operator with real credentials can trigger -- flips a row to
//! `Collected`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::pace::redact_for_log;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Where the manifest lives in a checkout, relative to the crate root.
pub const MANIFEST_RELATIVE_PATH: &str = "docs/evidence/provider-live-contract-manifest.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionStatus {
    NotCollected,
    Collected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRow {
    pub route: String,
    pub protocol: String,
    pub collection_status: CollectionStatus,
    #[serde(default)]
    pub collected_at_unix: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub asserted_behaviors: Vec<String>,
    #[serde(default)]
    pub outcome: Option<Outcome>,
    #[serde(default)]
    pub detail: Option<String>,
    /// The exact command an operator runs to collect this row. Never
    /// contains a real secret -- it names the environment variable, not a
    /// value.
    pub operator_command: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub routes: Vec<EvidenceRow>,
}

/// Every production route issue #592 names (N07, N08, N12, and the N13
/// OpenAI-compatible family's representative generic route), in fixed row
/// order, each carrying the exact command that collects it.
/// [`record_stream_result`] refuses a route name absent from this list
/// rather than silently appending one, so the manifest can never grow a row
/// nothing in this codebase actually produces.
fn template_rows() -> Vec<EvidenceRow> {
    let row = |route: &str, protocol: &str, operator_command: &str| EvidenceRow {
        route: route.to_string(),
        protocol: protocol.to_string(),
        collection_status: CollectionStatus::NotCollected,
        collected_at_unix: None,
        model: None,
        asserted_behaviors: Vec::new(),
        outcome: None,
        detail: None,
        operator_command: operator_command.to_string(),
    };
    vec![
        row(
            "anthropic-messages",
            "Anthropic Messages (N07, #476)",
            "ANTHROPIC_API_KEY=*** ZIRV_ANTHROPIC_LIVE_MODEL=<exact-model-id> \
             cargo test --bin zirv --release -- --ignored live_anthropic_messages_contract \
             --nocapture",
        ),
        row(
            "openai-responses",
            "OpenAI Responses (N08, #477)",
            "OPENAI_API_KEY=*** ZIRV_OPENAI_LIVE_MODEL=<exact-model-id> \
             cargo test --bin zirv --release -- --ignored live_openai_responses_contract \
             --nocapture",
        ),
        row(
            "google-developer",
            "Google Generative AI, Gemini Developer API (N12, #481)",
            "GEMINI_API_KEY=*** ZIRV_GOOGLE_LIVE_MODEL=<exact-model-id> \
             cargo test --bin zirv --release -- --ignored live_google_generative_ai_contract \
             --nocapture",
        ),
        row(
            "openai-chat-generic",
            "OpenAI-compatible chat completions (N13, #482)",
            "ZIRV_OPENAI_COMPATIBLE_BASE_URL=https://api.<vendor>.com \
             ZIRV_OPENAI_COMPATIBLE_API_KEY=*** ZIRV_OPENAI_COMPATIBLE_LIVE_MODEL=<exact-model-id> \
             cargo test --bin zirv --release -- --ignored live_openai_compatible_chat_contract \
             --nocapture",
        ),
    ]
}

/// The manifest a fresh checkout ships: every row present, every row
/// stating `NotCollected`.
pub fn template() -> Manifest {
    Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        routes: template_rows(),
    }
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(MANIFEST_RELATIVE_PATH)
}

fn load(path: &Path) -> CtxResult<Manifest> {
    if !path.is_file() {
        return Ok(template());
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Rewrites exactly one route's row with a freshly collected result against
/// the manifest at `path` and writes the whole file back. Only a route
/// already named in [`template_rows`] may be updated. Free text (`detail`)
/// is redacted before it is stored; the model id is recorded as given
/// (model ids are not secrets, unlike the credential that authenticated the
/// call, which this function never receives).
fn record_at(
    path: &Path,
    route: &str,
    model: &str,
    asserted_behaviors: &[&str],
    outcome: Outcome,
    detail: Option<&str>,
) -> CtxResult<()> {
    let mut manifest = load(path)?;
    let row = manifest
        .routes
        .iter_mut()
        .find(|row| row.route == route)
        .ok_or_else(|| format!("evidence manifest has no template row for route `{route}`"))?;
    row.collection_status = CollectionStatus::Collected;
    row.collected_at_unix = Some(crate::commands::ctx::state::now_secs());
    row.model = Some(model.to_string());
    row.asserted_behaviors = asserted_behaviors
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    row.outcome = Some(outcome);
    row.detail = detail.map(redact_for_log);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(&manifest)?;
    json.push('\n');
    std::fs::write(path, json)?;
    Ok(())
}

/// Convenience for a `live_*` contract test: records the outcome of one
/// `ProviderAdapter::stream` call against the manifest for `route`, then
/// returns the result unchanged so the calling test keeps making exactly
/// the assertions it always did (typically `.unwrap()`) and fails exactly
/// as it always would. This only ever adds a manifest write on the way
/// through -- it never turns a live failure into a passing test, and it
/// never turns a live pass into anything but what the adapter itself
/// reported.
///
/// `adapter` is the same adapter the caller just streamed through, and is
/// required precisely so a failure is always passed through its own
/// [`super::adapter::ProviderAdapter::redact_failure`] -- the exact-secret
/// scrub -- before the generic heuristic scrub runs on the way into the
/// manifest. There is no variant of this function that skips that step.
pub fn record_stream_result<T>(
    adapter: &dyn super::adapter::ProviderAdapter,
    route: &str,
    model: &str,
    asserted_behaviors: &[&str],
    result: Result<T, super::adapter::ProviderFailure>,
) -> Result<T, super::adapter::ProviderFailure> {
    record_stream_result_at(
        &manifest_path(),
        adapter,
        route,
        model,
        asserted_behaviors,
        result,
    )
}

fn record_stream_result_at<T>(
    path: &Path,
    adapter: &dyn super::adapter::ProviderAdapter,
    route: &str,
    model: &str,
    asserted_behaviors: &[&str],
    result: Result<T, super::adapter::ProviderFailure>,
) -> Result<T, super::adapter::ProviderFailure> {
    // Exact-secret scrub first (the adapter knows the literal credential),
    // defence-in-depth generic scrub second (`record_at` redacts `detail`
    // with `redact_for_log` regardless of what already ran here).
    let result = result.map_err(|failure| adapter.redact_failure(failure));
    match &result {
        Ok(_) => {
            record_at(path, route, model, asserted_behaviors, Outcome::Pass, None)
                .expect("write live-contract evidence manifest");
        }
        Err(failure) => {
            record_at(
                path,
                route,
                model,
                asserted_behaviors,
                Outcome::Fail,
                Some(&failure.to_string()),
            )
            .expect("write live-contract evidence manifest");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
    };

    /// A minimal [`super::super::adapter::ProviderAdapter`] test double.
    /// `redact_failure` mirrors what every real credentialed adapter
    /// (anthropic, openai, google, openai_chat) does: run the exact-secret
    /// scrub against its own known secrets. `stream` is never exercised --
    /// `record_stream_result`/`record_stream_result_at` only ever redact an
    /// already-produced `Result`, they never call `stream` themselves.
    #[derive(Debug)]
    struct FakeAdapter {
        secrets: Vec<&'static str>,
        target: super::super::adapter::ProviderTarget,
    }

    impl FakeAdapter {
        fn new(secrets: Vec<&'static str>) -> Self {
            Self {
                secrets,
                target: super::super::adapter::ProviderTarget {
                    route: RouteId::new("test-route").unwrap(),
                    provider: ProviderId::new("test-provider").unwrap(),
                    endpoint: EndpointId::new("test-endpoint").unwrap(),
                    account: AccountId::new("test-account").unwrap(),
                    billing_pool: BillingPoolId::new("test-pool").unwrap(),
                    protocol: Protocol::AnthropicMessages,
                    base_url: "https://example.invalid".to_string(),
                    model: ModelId {
                        vendor: "test".into(),
                        id: "test-model".into(),
                    },
                },
            }
        }
    }

    impl super::super::adapter::ProviderAdapter for FakeAdapter {
        fn protocol(&self) -> Protocol {
            self.target.protocol
        }

        fn target(&self) -> &super::super::adapter::ProviderTarget {
            &self.target
        }

        fn redact_failure(
            &self,
            failure: super::super::adapter::ProviderFailure,
        ) -> super::super::adapter::ProviderFailure {
            super::super::adapter::redact_failure(failure, &self.secrets)
        }

        fn stream(
            &self,
            _request: &super::super::adapter::ProviderRequest,
            _cancellation: &dyn super::super::adapter::Cancellation,
            _sink: &mut dyn super::super::adapter::EventSink,
        ) -> Result<super::super::adapter::ProviderResponse, super::super::adapter::ProviderFailure>
        {
            unreachable!(
                "record_stream_result never calls stream(); it only redacts an \
                 already-produced result"
            )
        }
    }

    #[test]
    fn an_unfilled_manifest_states_its_own_status_on_every_row() {
        let manifest = template();
        assert!(!manifest.routes.is_empty());
        for row in &manifest.routes {
            assert_eq!(row.collection_status, CollectionStatus::NotCollected);
            assert!(row.outcome.is_none());
            assert!(row.model.is_none());
            assert!(row.detail.is_none());
            assert!(!row.operator_command.is_empty());
        }
    }

    #[test]
    fn record_updates_only_the_named_route_and_redacts_free_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        record_at(
            &path,
            "anthropic-messages",
            "claude-example-model",
            &["single-turn completion", "usage tokens reported"],
            Outcome::Pass,
            Some("Bearer sk-should-not-appear ok"),
        )
        .expect("record");

        let manifest = load(&path).expect("load");
        let collected = manifest
            .routes
            .iter()
            .find(|row| row.route == "anthropic-messages")
            .expect("row present");
        assert_eq!(collected.collection_status, CollectionStatus::Collected);
        assert_eq!(collected.outcome, Some(Outcome::Pass));
        assert_eq!(collected.model.as_deref(), Some("claude-example-model"));
        assert_eq!(
            collected.asserted_behaviors,
            ["single-turn completion", "usage tokens reported"]
        );
        assert!(
            !collected
                .detail
                .as_deref()
                .expect("detail present")
                .contains("sk-should-not-appear"),
            "the secret-shaped word must be redacted: {:?}",
            collected.detail
        );

        let untouched = manifest
            .routes
            .iter()
            .find(|row| row.route == "openai-responses")
            .expect("other route rows stay present");
        assert_eq!(untouched.collection_status, CollectionStatus::NotCollected);
    }

    #[test]
    fn record_refuses_a_route_name_absent_from_the_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        let error = record_at(&path, "not-a-real-route", "m", &[], Outcome::Pass, None)
            .expect_err("unknown route must be refused");
        assert!(error.to_string().contains("no template row"), "{error}");
    }

    #[test]
    fn loading_a_missing_file_returns_the_template_rather_than_erroring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(load(&path).expect("load"), template());
    }

    #[test]
    fn record_stream_result_passes_results_through_unchanged() {
        use super::super::adapter::{FailureClass, FailureScope, ProviderFailure};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        let adapter = FakeAdapter::new(Vec::new());

        // A pass records and is returned unchanged.
        let passed = record_stream_result_at(
            &path,
            &adapter,
            "anthropic-messages",
            "m",
            &["b"],
            Ok::<_, ProviderFailure>(42),
        );
        assert_eq!(passed.unwrap(), 42);
        let manifest = load(&path).expect("load");
        let row = manifest
            .routes
            .iter()
            .find(|row| row.route == "anthropic-messages")
            .unwrap();
        assert_eq!(row.outcome, Some(Outcome::Pass));

        // A failure records too, and the original error still comes back --
        // this must never turn a live failure into a passing test.
        let failure = ProviderFailure::new(
            FailureClass::Authentication,
            FailureScope::request(),
            "bad key",
        );
        let result = record_stream_result_at(
            &path,
            &adapter,
            "anthropic-messages",
            "m",
            &["b"],
            Err::<(), _>(failure),
        );
        assert_eq!(result.unwrap_err().class, FailureClass::Authentication);
        let manifest = load(&path).expect("load");
        let row = manifest
            .routes
            .iter()
            .find(|row| row.route == "anthropic-messages")
            .unwrap();
        assert_eq!(row.outcome, Some(Outcome::Fail));
    }

    #[test]
    fn record_stream_result_scrubs_a_credential_shape_the_generic_heuristic_alone_misses() {
        use super::super::adapter::{FailureClass, FailureScope, ProviderFailure};

        // A bare hex token with no `Bearer`/`key=`/`token=` marker, no
        // `sk-`/`ghp_`/`gho_` prefix and no `@` is exactly the shape
        // `pace::redact_for_log`'s word-by-word heuristic does not
        // recognise on its own (see `redact_word` in `pace.rs`) -- the
        // shape a self-hosted OpenAI-compatible proxy (vLLM, LM Studio,
        // ...) can echo back verbatim in a JSON `error.message`.
        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let body = format!("upstream rejected credential {secret} in request");
        assert_eq!(
            redact_for_log(&body),
            body,
            "the generic heuristic must not already catch this shape, or this test proves nothing"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        let adapter = FakeAdapter::new(vec![secret]);
        let failure =
            ProviderFailure::new(FailureClass::Authentication, FailureScope::request(), body);

        let result = record_stream_result_at(
            &path,
            &adapter,
            "anthropic-messages",
            "m",
            &["b"],
            Err::<(), _>(failure),
        );
        assert!(result.is_err(), "a live failure must still fail the caller");

        let manifest = load(&path).expect("load");
        let row = manifest
            .routes
            .iter()
            .find(|row| row.route == "anthropic-messages")
            .unwrap();
        let detail = row.detail.as_deref().expect("detail present");
        assert!(
            !detail.contains(secret),
            "the exact credential must never reach the committed manifest: {detail:?}"
        );
    }
}
