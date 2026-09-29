//! Provider error classification: recognizing overflow, rate-limit, transport,
//! server, and auth wording in a provider's own error text.
use super::*;

/// Empirical overflow wording for the two provider families zirv supervises.
/// These are the Anthropic and OpenAI entries from pi's live-provider table;
/// broader provider entries stay out until zirv can observe those providers.
const PROVIDER_OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",
    "request_too_large",
    "exceeds the context window",
    "maximum context length",
];

/// Checked before overflow wording. In particular, Bedrock throttling can say
/// "too many tokens", while a rate-limit response can quote the rejected
/// request, so a later overflow-shaped fragment must never win.
const PROVIDER_RATE_LIMIT_PREFIXES: &[&str] = &["throttling error:"];

const PROVIDER_RATE_LIMIT_PATTERNS: &[&str] = &["rate limit", "too many requests"];

/// Classify transport reachability from the harness's explicit connection-failure wording. (#455)
const PROVIDER_TRANSPORT_PATTERNS: &[&str] = &[
    "connection refused",
    "connection reset",
    "connectionrefused",
    "connectionreset",
    "econnrefused",
    "econnreset",
    "econnaborted",
    "etimedout",
    "enotfound",
    "eai_again",
    "firewall or proxy",
    "socket hang up",
    "fetch failed",
    "network error",
    "dns error",
    "dns failure",
    "getaddrinfo",
    "request timed out",
    "request timeout",
    "timed out waiting for the first token",
];

/// Classify explicit provider failure before quoted overflow wording; a bare status number is not evidence. (#455)
const PROVIDER_SERVER_PREFIXES: &[&str] = &["service unavailable:"];

const PROVIDER_SERVER_PATTERNS: &[&str] = &[
    "overloaded",
    "internal server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "upstream connect error",
];

/// Match explicit provider rejection only; sandbox wording and bare status numbers must not suppress cooldown. (#455)
const PROVIDER_AUTH_PATTERNS: &[&str] = &[
    "authentication_error",
    "invalid_api_key",
    "invalid api key",
    "invalid x-api-key",
    "permission_error",
];

/// The structured fields a provider-error row carries alongside its text.
/// Claude Code's transcript rows hold both (`error: "server_error"`,
/// `apiErrorStatus: 429`); codex's `task_complete` payload holds neither, so
/// [`ProviderErrorHints::default`] is the text-only case.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProviderErrorHints<'a> {
    /// The row's own error-kind string (`"server_error"`, `"rate_limit"`).
    pub kind: Option<&'a str>,
    /// The row's own HTTP status.
    pub status: Option<u64>,
}

/// Classifies one provider error.
///
/// Specific text patterns always win: they are the only evidence that
/// distinguishes an overflow (`prompt is too long`) from the generic
/// `server_error` kind Claude Code stamps on nearly everything. `hints` is
/// consulted only when no pattern matched, so a structured field can never
/// overrule observed wording.
pub(crate) fn classify_provider_error(
    message: &str,
    hints: ProviderErrorHints<'_>,
) -> ProviderErrorClass {
    let lowered = message.trim_start().to_lowercase();
    if PROVIDER_RATE_LIMIT_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
        || PROVIDER_RATE_LIMIT_PATTERNS
            .iter()
            .any(|pattern| lowered.contains(pattern))
    {
        return ProviderErrorClass::RateLimit;
    }
    if PROVIDER_TRANSPORT_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
    {
        return ProviderErrorClass::Transport;
    }
    if PROVIDER_SERVER_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
        || PROVIDER_SERVER_PATTERNS
            .iter()
            .any(|pattern| lowered.contains(pattern))
    {
        return ProviderErrorClass::Server;
    }
    if PROVIDER_AUTH_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
    {
        return ProviderErrorClass::Auth;
    }
    if PROVIDER_OVERFLOW_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
    {
        return ProviderErrorClass::Overflow;
    }
    classify_from_hints(hints)
}

/// Deduplicate provider-error rows by timestamp and message fingerprint; without a timestamp, retries must remain distinct. (#455)
pub(crate) fn provider_error_id(at_ms: Option<u64>, message: &str) -> Option<String> {
    let at_ms = at_ms?;
    Some(format!(
        "{at_ms}-{:016x}",
        super::super::event::input_hash(message.trim())
    ))
}

/// Cap on an [`summary`](super::super::event::UnresolvedToolCall::summary) line and
/// on the error detail folded into `StructuralContext::tail_cut`'s reason
/// text (issue #455): long enough to show a real command, path or error
/// message, short enough that a pathological one (a huge inline patch, a
/// giant error body) cannot make the handoff arbitrarily large.
pub(crate) const TOOL_CALL_SUMMARY_CAP: usize = 120;

/// Reduces `raw` to a single, bounded, secret-scrubbed line (issue #455):
/// redacted through [`super::super::pace::redact_for_log`] (which also collapses it
/// to one line), then cut to [`TOOL_CALL_SUMMARY_CAP`] characters. Used for
/// an unresolved tool call's own summary and for the error text folded into
/// a `tail_cut` reason -- both are transcript-derived text that may be
/// arbitrarily large or carry a secret-shaped token, and neither has ever
/// been screened before reaching a successor session's prompt.
pub(crate) fn redacted_tool_summary(raw: &str) -> String {
    let redacted = super::super::pace::redact_for_log(raw);
    let chars: Vec<char> = redacted.chars().collect();
    if chars.len() <= TOOL_CALL_SUMMARY_CAP {
        return redacted;
    }
    let truncated: String = chars[..TOOL_CALL_SUMMARY_CAP.saturating_sub(3)]
        .iter()
        .collect();
    format!("{truncated}...")
}

fn classify_from_hints(hints: ProviderErrorHints<'_>) -> ProviderErrorClass {
    if let Some(status) = hints.status {
        match status {
            429 => return ProviderErrorClass::RateLimit,
            401 | 403 | 404 => return ProviderErrorClass::Auth,
            500..=599 => return ProviderErrorClass::Server,
            _ => {}
        }
    }
    match hints.kind.map(str::to_lowercase).as_deref() {
        Some("rate_limit" | "rate_limit_error") => ProviderErrorClass::RateLimit,
        Some("server_error" | "api_error" | "overloaded_error") => ProviderErrorClass::Server,
        Some(
            "authentication_error" | "permission_error" | "invalid_api_key" | "not_found_error",
        ) => ProviderErrorClass::Auth,
        _ => ProviderErrorClass::Other,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn api_error_classification_checks_exclusions_before_overflow_patterns() {
        use crate::commands::ctx::event::ProviderErrorClass;

        let cases = [
            (
                "API Error: prompt is too long: 213462 tokens > 200000 maximum",
                ProviderErrorClass::Overflow,
            ),
            (
                "API Error: 413 {\"error\":{\"type\":\"request_too_large\"}}",
                ProviderErrorClass::Overflow,
            ),
            (
                "Your input exceeds the context window of this model",
                ProviderErrorClass::Overflow,
            ),
            (
                "Requested token count exceeds the model's maximum context length of 131072 tokens",
                ProviderErrorClass::Overflow,
            ),
            (
                "Throttling error: Too many tokens, please wait before trying again",
                ProviderErrorClass::RateLimit,
            ),
            (
                "rate limit: prompt is too long",
                ProviderErrorClass::RateLimit,
            ),
            (
                "too many requests: request_too_large",
                ProviderErrorClass::RateLimit,
            ),
            // Issue #455: reachability wording no longer lands in the
            // catch-all -- `service unavailable:` is the provider failing,
            // a reset connection never reached it at all.
            (
                "Service unavailable: request_too_large",
                ProviderErrorClass::Server,
            ),
            ("API Error: connection reset", ProviderErrorClass::Transport),
        ];

        for (message, expected) in cases {
            assert_eq!(
                super::super::classify_provider_error(
                    message,
                    super::super::ProviderErrorHints::default()
                ),
                expected,
                "{message}"
            );
        }
    }

    /// Issue #455: the reachability classes, including the exact wording the
    /// observed incident produced, and the structured-hint fallback for a
    /// row whose text says nothing specific.
    #[test]
    fn reachability_errors_are_classified_from_text_then_from_structured_hints() {
        use crate::commands::ctx::adapters::ProviderErrorHints;
        use crate::commands::ctx::event::ProviderErrorClass;

        let text_cases = [
            (
                "API Error: Connection refused - a firewall or proxy may be blocking it \
                 (ConnectionRefused)",
                ProviderErrorClass::Transport,
            ),
            (
                "API Error: 503 Service Unavailable",
                ProviderErrorClass::Server,
            ),
            ("overloaded_error", ProviderErrorClass::Server),
            (
                "API Error: 401 authentication_error",
                ProviderErrorClass::Auth,
            ),
        ];
        for (message, expected) in text_cases {
            assert_eq!(
                super::super::classify_provider_error(message, ProviderErrorHints::default()),
                expected,
                "{message}"
            );
        }

        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints {
                    kind: Some("server_error"),
                    status: None,
                }
            ),
            ProviderErrorClass::Server,
            "a neutral message with error: server_error is a server failure"
        );
        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints {
                    kind: Some("server_error"),
                    status: Some(429),
                }
            ),
            ProviderErrorClass::RateLimit,
            "the status wins over the generic kind"
        );
        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints::default()
            ),
            ProviderErrorClass::Other,
            "no text match and no hints stays unattributed"
        );
    }

    /// Review round 1, finding 3: `Auth` is the one class no cooldown used
    /// to clear, and it was reachable from ordinary English. A sandbox
    /// refusing a file write and a transient proxy 404 are not credential
    /// problems, and codex's `task_complete.error.message` is task text.
    #[test]
    fn ordinary_english_and_bare_status_numbers_never_classify_as_auth() {
        use crate::commands::ctx::adapters::ProviderErrorHints;
        use crate::commands::ctx::event::ProviderErrorClass;

        for message in [
            "permission denied writing /x",
            "404 Not Found",
            "EACCES: permission denied, open '/etc/hosts'",
            "the tool returned 401 lines of output",
        ] {
            assert_eq!(
                super::super::classify_provider_error(message, ProviderErrorHints::default()),
                ProviderErrorClass::Other,
                "{message}"
            );
        }

        // The status still reaches `Auth` -- through the structured field,
        // which is the only place a number is evidence.
        assert_eq!(
            super::super::classify_provider_error(
                "the request failed",
                ProviderErrorHints {
                    kind: None,
                    status: Some(401),
                }
            ),
            ProviderErrorClass::Auth
        );
        // A bare 5xx in prose is likewise not evidence on its own; the
        // words are.
        assert_eq!(
            super::super::classify_provider_error(
                "upstream said 500",
                ProviderErrorHints::default()
            ),
            ProviderErrorClass::Other
        );
        assert_eq!(
            super::super::classify_provider_error(
                "internal server error",
                ProviderErrorHints::default()
            ),
            ProviderErrorClass::Server
        );
    }
}
