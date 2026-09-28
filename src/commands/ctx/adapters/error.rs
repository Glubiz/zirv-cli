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

/// Issue #455: the request never reached the provider. `"firewall or proxy"`
/// and `"ConnectionRefused"` are Claude Code's own wording for the incident
/// this classification exists for (`API Error: Connection refused -- a
/// firewall or proxy may be blocking it (ConnectionRefused)`).
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

/// It reached the provider and the provider failed. Checked before the
/// overflow wording, like the rate-limit list above and for the same reason:
/// a 5xx body can quote the rejected request.
///
/// Review round 1, finding 3: bare numeric status fragments (`" 503 "`) are
/// deliberately NOT here. A number in prose is not evidence -- an HTTP
/// status reaches this function through `ProviderErrorHints::status`, which
/// is a structured field, and codex's `task_complete.error.message` is task
/// text that can contain any number at all.
const PROVIDER_SERVER_PREFIXES: &[&str] = &["service unavailable:"];

const PROVIDER_SERVER_PATTERNS: &[&str] = &[
    "overloaded",
    "internal server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "upstream connect error",
];

/// The provider rejected the caller rather than failing.
///
/// Finding 3: only explicit PROVIDER tokens, never English that an agent's
/// own task text produces. `"permission denied"` is what a sandbox says
/// about a file, and a bare `404` is as likely a transient proxy as a wrong
/// model id -- both used to read as `Auth`, which is the one class no
/// cooldown was allowed to clear. Everything else arrives as a structured
/// hint (see [`classify_from_hints`]).
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

/// Issue #455: a stable identity for one provider-error row whose transcript
/// shape carries none of its own -- the row's own millisecond timestamp plus
/// a fingerprint of its message.
///
/// `None` without a timestamp: consecutive retries of one failing turn carry
/// the SAME message text, so content alone would collapse three real
/// failures into one observation. With no time to separate them, no identity
/// is honest, and `health::observe` simply does not de-duplicate.
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
