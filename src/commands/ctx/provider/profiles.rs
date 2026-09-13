//! Versioned route-profile registry (issue #482, roadmap N13).
//!
//! A [`ProviderSpec`](super::ProviderSpec) answers "which wire protocol and
//! which auth header". A [`RouteProfile`] answers the rest of the questions a
//! concrete vendor route raises: which documented base URL and request path,
//! which credential *class* (including "none at all" for a local server),
//! which capabilities the vendor actually implements behind a compatible
//! protocol, and which provider-native request options may be set.
//!
//! The registry is data, not code: adding a vendor or a local runtime is a
//! row here plus (only when the wire protocol itself is new) a transport
//! module. Nothing in the agent loop, the TUI or the tool layer changes.
//!
//! Two rules keep the registry honest:
//!
//! 1. Every accessible route is bound to a profile. A route whose vendor has
//!    no row is refused at configuration time rather than sent to a guessed
//!    endpoint.
//! 2. [`Support::LegacyOnly`] is reserved for an *upstream* limitation -- a
//!    broker subscription with no documented, authorized direct API. It is
//!    never used for "zirv has not written that adapter yet", which is
//!    [`Support::Planned`].

use std::collections::BTreeMap;

use serde::Serialize;

use super::{Protocol, Support};

/// The registry's own schema version. A profile row's `version` is bumped
/// when that row's meaning changes (a new documented base URL, a credential
/// class change); this constant is bumped when the *shape* of a row changes.
pub const PROFILE_SCHEMA: u32 = 1;

/// Where a profile's base URL comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BaseUrl {
    /// The vendor publishes one base URL. An endpoint may still override it
    /// (a regional mirror, a proxy), but omitting `base_url` is legal.
    Documented { url: &'static str },
    /// No documented global base: the operator's own host, deployment or
    /// region decides it, so `base_url` is required.
    Operator,
}

impl BaseUrl {
    pub fn default_url(self) -> Option<&'static str> {
        match self {
            Self::Documented { url } => Some(url),
            Self::Operator => None,
        }
    }
}

/// How a route authenticates. This is the *class* of secret, which is what
/// decides whether a credential may be fabricated, reused or omitted -- the
/// header spelling itself lives on the [`ProviderSpec`](super::ProviderSpec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CredentialClass {
    /// `authorization: Bearer <key>`: a vendor-issued API key.
    BearerApiKey,
    /// A vendor header carrying the key verbatim (`x-api-key`, `api-key`).
    HeaderApiKey { header: &'static str },
    /// A Google API key in `x-goog-api-key`.
    GoogleApiKey,
    /// A short-lived OAuth access token the operator mints elsewhere.
    OAuthAccessToken,
    /// An AWS access-key pair signed per request with SigV4.
    AwsSigV4,
    /// The endpoint takes no credential. Only legal for a local runtime on a
    /// loopback or private host: nothing is ever fabricated to fill this in.
    LocalNone,
    /// A local runtime that may or may not have been started with a key.
    LocalOptional,
    /// A broker subscription identity. Never a vendor API credential.
    BrokerSubscription,
}

impl CredentialClass {
    /// Whether a route of this class may run with no credential resolved.
    pub fn is_optional(self) -> bool {
        matches!(self, Self::LocalNone | Self::LocalOptional)
    }

    /// Whether this class is only legitimate against a local host.
    pub fn is_local(self) -> bool {
        matches!(self, Self::LocalNone | Self::LocalOptional)
    }
}

/// What the vendor actually implements behind its chosen protocol. A
/// compatible endpoint speaking `chat.completions` is not automatically a
/// vision, caching or reasoning endpoint, and saying so here is what lets a
/// request carrying an unsupported requirement be rejected instead of
/// silently stripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Caveats {
    pub tools: bool,
    /// Tool arguments arrive as incremental deltas rather than one blob.
    pub streamed_tool_args: bool,
    pub vision: bool,
    pub structured_output: bool,
    pub prompt_caching: bool,
    /// The endpoint accepts a reasoning-effort control.
    pub reasoning_controls: bool,
    /// Reasoning text can be replayed as continuation material on the next
    /// turn. `false` means any reasoning the endpoint emits is display-only.
    pub reasoning_replay: bool,
    pub parallel_tool_calls: bool,
    pub notes: &'static [&'static str],
}

const PLAIN_CHAT: Caveats = Caveats {
    tools: true,
    streamed_tool_args: true,
    vision: false,
    structured_output: true,
    prompt_caching: false,
    reasoning_controls: false,
    reasoning_replay: false,
    parallel_tool_calls: true,
    notes: &[],
};

/// A provider-native request option this profile accepts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ExtensionSpec {
    pub key: &'static str,
    pub value: ExtensionType,
    pub note: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ExtensionType {
    Bool,
    Integer { min: i64, max: i64 },
    Number { min: f64, max: f64 },
    String,
    Enum { values: &'static [&'static str] },
}

/// One route profile: everything a configured route needs beyond its
/// protocol.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RouteProfile {
    pub id: &'static str,
    pub version: u32,
    /// The `provider` id an account must name to reach this profile.
    pub provider: &'static str,
    /// The catalogue vendor slug this profile serves, or `None` for the
    /// generic fallback that any otherwise-unknown compatible vendor gets.
    pub vendor: Option<&'static str>,
    pub protocol: Protocol,
    pub base_url: BaseUrl,
    /// The request path appended to the base URL. Azure and Bedrock build
    /// their path from per-account identity instead, and carry `""`.
    pub path: &'static str,
    pub credential: CredentialClass,
    /// Environment variables the vendor documents for this credential.
    pub credential_env: &'static [&'static str],
    pub caveats: Caveats,
    pub extensions: &'static [ExtensionSpec],
    pub support: Support,
    pub doc: &'static str,
}

impl RouteProfile {
    /// Whether a plain-HTTP base URL is defensible for this profile. Only a
    /// local runtime qualifies, and even then the endpoint host still has to
    /// be loopback or private (`probe::is_local_http_host`).
    pub fn allows_plain_http(&self) -> bool {
        self.credential.is_local()
    }
}

// -- common extension sets ----------------------------------------------

/// Sampling controls the OpenAI chat-completions specification itself
/// defines, which every compatible vendor in this registry documents.
const CHAT_SAMPLING: &[ExtensionSpec] = &[
    ExtensionSpec {
        key: "temperature",
        value: ExtensionType::Number { min: 0.0, max: 2.0 },
        note: "OpenAI chat-completions sampling temperature",
    },
    ExtensionSpec {
        key: "top_p",
        value: ExtensionType::Number { min: 0.0, max: 1.0 },
        note: "OpenAI chat-completions nucleus sampling",
    },
    ExtensionSpec {
        key: "seed",
        value: ExtensionType::Integer {
            min: 0,
            max: i64::MAX,
        },
        note: "best-effort determinism where the vendor implements it",
    },
];

const QWEN_EXTENSIONS: &[ExtensionSpec] = &[
    ExtensionSpec {
        key: "temperature",
        value: ExtensionType::Number { min: 0.0, max: 2.0 },
        note: "OpenAI chat-completions sampling temperature",
    },
    ExtensionSpec {
        key: "top_p",
        value: ExtensionType::Number { min: 0.0, max: 1.0 },
        note: "OpenAI chat-completions nucleus sampling",
    },
    ExtensionSpec {
        key: "enable_thinking",
        value: ExtensionType::Bool,
        note: "DashScope compatible-mode thinking switch for the Qwen3 family",
    },
];

const VLLM_EXTENSIONS: &[ExtensionSpec] = &[
    ExtensionSpec {
        key: "temperature",
        value: ExtensionType::Number { min: 0.0, max: 2.0 },
        note: "OpenAI chat-completions sampling temperature",
    },
    ExtensionSpec {
        key: "top_k",
        value: ExtensionType::Integer { min: -1, max: 1000 },
        note: "vLLM sampling extension (not part of the OpenAI schema)",
    },
    ExtensionSpec {
        key: "repetition_penalty",
        value: ExtensionType::Number { min: 0.0, max: 2.0 },
        note: "vLLM sampling extension (not part of the OpenAI schema)",
    },
];

// -- the registry --------------------------------------------------------

const OPENAI_CHAT_PATH: &str = "/v1/chat/completions";

pub static PROFILES: &[RouteProfile] = &[
    RouteProfile {
        id: "anthropic-messages",
        version: 1,
        provider: "anthropic",
        vendor: Some("anthropic"),
        protocol: Protocol::AnthropicMessages,
        base_url: BaseUrl::Documented {
            url: "https://api.anthropic.com",
        },
        path: "/v1/messages",
        credential: CredentialClass::HeaderApiKey {
            header: "x-api-key",
        },
        credential_env: &["ANTHROPIC_API_KEY"],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: true,
            structured_output: true,
            prompt_caching: true,
            reasoning_controls: true,
            reasoning_replay: true,
            parallel_tool_calls: true,
            notes: &["thinking blocks replay verbatim with their signature"],
        },
        extensions: &[],
        support: Support::Native,
        doc: "Anthropic Messages API (N07)",
    },
    RouteProfile {
        id: "openai-responses",
        version: 1,
        provider: "openai",
        vendor: Some("openai"),
        protocol: Protocol::OpenAiResponses,
        base_url: BaseUrl::Documented {
            url: "https://api.openai.com",
        },
        path: "/v1/responses",
        credential: CredentialClass::BearerApiKey,
        credential_env: &["OPENAI_API_KEY"],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: true,
            structured_output: true,
            prompt_caching: true,
            reasoning_controls: true,
            reasoning_replay: true,
            parallel_tool_calls: true,
            notes: &["encrypted reasoning items are the continuation material"],
        },
        extensions: &[],
        support: Support::Native,
        doc: "OpenAI Responses API (N08)",
    },
    RouteProfile {
        id: "google-developer",
        version: 1,
        provider: "google",
        vendor: Some("google"),
        protocol: Protocol::GoogleGenerativeAi,
        base_url: BaseUrl::Documented {
            url: "https://generativelanguage.googleapis.com",
        },
        path: "/v1beta/models",
        credential: CredentialClass::GoogleApiKey,
        credential_env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: true,
            structured_output: true,
            prompt_caching: false,
            reasoning_controls: true,
            reasoning_replay: true,
            parallel_tool_calls: true,
            notes: &["thought signatures are the continuation material"],
        },
        extensions: &[],
        support: Support::Native,
        doc: "Gemini Developer API (N12)",
    },
    RouteProfile {
        id: "google-vertex",
        version: 1,
        provider: "google-vertex",
        vendor: Some("google"),
        protocol: Protocol::GoogleVertex,
        base_url: BaseUrl::Operator,
        path: "",
        credential: CredentialClass::OAuthAccessToken,
        credential_env: &[],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: true,
            structured_output: true,
            prompt_caching: false,
            reasoning_controls: true,
            reasoning_replay: true,
            parallel_tool_calls: true,
            notes: &["addressed by project and location, not by a bare key"],
        },
        extensions: &[],
        support: Support::Native,
        doc: "Vertex AI publisher models (N12)",
    },
    RouteProfile {
        id: "azure-openai-chat",
        version: 1,
        provider: "azure-openai",
        vendor: Some("openai"),
        protocol: Protocol::AzureOpenAiChat,
        base_url: BaseUrl::Operator,
        path: "",
        credential: CredentialClass::HeaderApiKey { header: "api-key" },
        credential_env: &["AZURE_OPENAI_API_KEY"],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: true,
            structured_output: true,
            prompt_caching: false,
            reasoning_controls: false,
            reasoning_replay: false,
            parallel_tool_calls: true,
            notes: &[
                "addressed by deployment id and api-version, not by model id",
                "a deployment's served model is an account fact zirv cannot infer",
            ],
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "Azure OpenAI chat completions (N13)",
    },
    RouteProfile {
        id: "aws-bedrock-anthropic",
        version: 1,
        provider: "aws-bedrock",
        vendor: Some("anthropic"),
        protocol: Protocol::AwsBedrock,
        base_url: BaseUrl::Operator,
        path: "",
        credential: CredentialClass::AwsSigV4,
        credential_env: &[],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: true,
            structured_output: true,
            prompt_caching: false,
            reasoning_controls: true,
            reasoning_replay: true,
            parallel_tool_calls: true,
            notes: &[
                "Anthropic Messages body without `model`, plus anthropic_version",
                "responses arrive as AWS event-stream frames, not SSE",
            ],
        },
        extensions: &[],
        support: Support::Native,
        doc: "Anthropic models on Amazon Bedrock (N13)",
    },
    RouteProfile {
        id: "aws-bedrock-converse",
        version: 1,
        provider: "aws-bedrock",
        vendor: None,
        protocol: Protocol::AwsBedrock,
        base_url: BaseUrl::Operator,
        path: "",
        credential: CredentialClass::AwsSigV4,
        credential_env: &[],
        caveats: Caveats {
            tools: true,
            streamed_tool_args: true,
            vision: false,
            structured_output: true,
            prompt_caching: false,
            reasoning_controls: false,
            reasoning_replay: false,
            parallel_tool_calls: true,
            notes: &[
                "Converse is Bedrock's vendor-neutral body; Amazon Nova and \
                 the Bedrock-hosted Meta, Mistral and Cohere families use it",
                "reasoningContent is display-only here and is never replayed",
            ],
        },
        extensions: &[],
        support: Support::Native,
        doc: "Amazon Bedrock Converse (N13)",
    },
    // -- OpenAI chat-completions-compatible vendors -----------------------
    RouteProfile {
        id: "deepseek-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("deepseek"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://api.deepseek.com",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &["DEEPSEEK_API_KEY"],
        caveats: Caveats {
            reasoning_controls: false,
            notes: &["deepseek-reasoner emits `reasoning_content`, which this \
                 transport surfaces as stream-only thinking and never replays"],
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "DeepSeek OpenAI-compatible chat completions",
    },
    RouteProfile {
        id: "xai-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("xai"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://api.x.ai",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &["XAI_API_KEY"],
        caveats: Caveats {
            vision: true,
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "xAI Grok OpenAI-compatible chat completions",
    },
    RouteProfile {
        id: "qwen-dashscope-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("qwen"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://dashscope-intl.aliyuncs.com/compatible-mode",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &["DASHSCOPE_API_KEY"],
        caveats: Caveats {
            vision: true,
            notes: &["the mainland base is dashscope.aliyuncs.com/compatible-mode"],
            ..PLAIN_CHAT
        },
        extensions: QWEN_EXTENSIONS,
        support: Support::Native,
        doc: "Alibaba DashScope compatible mode (Qwen)",
    },
    RouteProfile {
        id: "moonshot-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("moonshot"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://api.moonshot.ai",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &["MOONSHOT_API_KEY"],
        caveats: Caveats {
            vision: true,
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "Moonshot (Kimi) OpenAI-compatible chat completions",
    },
    RouteProfile {
        id: "mistral-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("mistral"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://api.mistral.ai",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &["MISTRAL_API_KEY"],
        caveats: Caveats {
            vision: true,
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "Mistral OpenAI-compatible chat completions",
    },
    RouteProfile {
        id: "zhipu-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("zhipu"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://open.bigmodel.cn/api/paas/v4",
        },
        path: "/chat/completions",
        credential: CredentialClass::BearerApiKey,
        credential_env: &["ZHIPUAI_API_KEY"],
        caveats: Caveats {
            vision: true,
            notes: &["the international base is api.z.ai/api/paas/v4"],
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "Zhipu GLM open-platform chat completions",
    },
    RouteProfile {
        id: "minimax-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("minimax"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://api.minimax.io",
        },
        path: "/v1/text/chatcompletion_v2",
        credential: CredentialClass::BearerApiKey,
        credential_env: &["MINIMAX_API_KEY"],
        caveats: Caveats {
            notes: &["MiniMax serves the compatible body at its own path"],
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "MiniMax chat completion v2",
    },
    RouteProfile {
        id: "meta-llama-chat",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("meta"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "https://api.llama.com/compat",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &["LLAMA_API_KEY"],
        caveats: Caveats {
            vision: true,
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "Meta Llama API compatibility layer",
    },
    // -- local runtimes ---------------------------------------------------
    RouteProfile {
        id: "ollama-openai",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("ollama"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "http://127.0.0.1:11434",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::LocalNone,
        credential_env: &[],
        caveats: Caveats {
            structured_output: false,
            notes: &[
                "Ollama's own /api/chat is not used: its OpenAI-compatible \
                 /v1 surface is the documented one and needs no second parser",
                "tool support depends on the pulled model, not on the server",
            ],
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "Ollama OpenAI-compatible surface",
    },
    RouteProfile {
        id: "lmstudio-openai",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("lmstudio"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "http://127.0.0.1:1234",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::LocalNone,
        credential_env: &[],
        caveats: Caveats {
            structured_output: false,
            notes: &["tool support depends on the loaded model"],
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "LM Studio local server",
    },
    RouteProfile {
        id: "vllm-openai",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("vllm"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Documented {
            url: "http://127.0.0.1:8000",
        },
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::LocalOptional,
        credential_env: &["VLLM_API_KEY"],
        caveats: Caveats {
            notes: &["tool calling requires the server's own --enable-auto-tool-choice"],
            ..PLAIN_CHAT
        },
        extensions: VLLM_EXTENSIONS,
        support: Support::Native,
        doc: "vLLM OpenAI-compatible server",
    },
    // -- broker subscriptions ---------------------------------------------
    RouteProfile {
        id: "copilot-broker",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("copilot"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Operator,
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BrokerSubscription,
        credential_env: &[],
        caveats: Caveats {
            tools: false,
            streamed_tool_args: false,
            structured_output: false,
            parallel_tool_calls: false,
            ..PLAIN_CHAT
        },
        extensions: &[],
        support: Support::LegacyOnly(
            "GitHub Copilot is a subscription brokered through the Copilot \
             editor/CLI identity; its model endpoint is not a documented, \
             separately authorized direct API, so zirv spends it through the \
             Copilot harness backend instead of minting a vendor identity",
        ),
        doc: "GitHub Copilot (harness backend only)",
    },
    RouteProfile {
        id: "factory-droid-broker",
        version: 1,
        provider: "openai-compatible",
        vendor: Some("factory"),
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Operator,
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BrokerSubscription,
        credential_env: &[],
        caveats: Caveats {
            tools: false,
            streamed_tool_args: false,
            structured_output: false,
            parallel_tool_calls: false,
            ..PLAIN_CHAT
        },
        extensions: &[],
        support: Support::LegacyOnly(
            "Factory/Droid resells upstream models under its own subscription \
             identity; there is no documented direct API a third-party client \
             may authenticate against, so the Droid harness backend stays the \
             way to spend it",
        ),
        doc: "Factory Droid (harness backend only)",
    },
    // -- the generic fallback ---------------------------------------------
    RouteProfile {
        id: "openai-chat-generic",
        version: 1,
        provider: "openai-compatible",
        vendor: None,
        protocol: Protocol::OpenAiChatCompatible,
        base_url: BaseUrl::Operator,
        path: OPENAI_CHAT_PATH,
        credential: CredentialClass::BearerApiKey,
        credential_env: &[],
        caveats: Caveats {
            structured_output: false,
            notes: &["an operator-declared compatible endpoint: nothing beyond \
                 streamed chat completions and tool calls is assumed"],
            ..PLAIN_CHAT
        },
        extensions: CHAT_SAMPLING,
        support: Support::Native,
        doc: "generic OpenAI chat-completions-compatible endpoint",
    },
];

/// Every catalogue vendor's own direct route, named explicitly so that a new
/// family cannot be added to the catalogue and quietly inherit the generic
/// compatible fallback. `amazon` resolves through Bedrock Converse because
/// the Nova family has no separate direct API.
const VENDOR_ROUTES: &[(&str, &str)] = &[
    ("anthropic", "anthropic-messages"),
    ("openai", "openai-responses"),
    ("google", "google-developer"),
    ("xai", "xai-chat"),
    ("qwen", "qwen-dashscope-chat"),
    ("moonshot", "moonshot-chat"),
    ("mistral", "mistral-chat"),
    ("deepseek", "deepseek-chat"),
    ("zhipu", "zhipu-chat"),
    ("minimax", "minimax-chat"),
    ("meta", "meta-llama-chat"),
    ("amazon", "aws-bedrock-converse"),
    ("ollama", "ollama-openai"),
    ("lmstudio", "lmstudio-openai"),
    ("vllm", "vllm-openai"),
];

/// The documented direct route for a catalogue vendor.
pub fn vendor_profile(slug: &str) -> Option<&'static RouteProfile> {
    VENDOR_ROUTES
        .iter()
        .find(|(vendor, _)| *vendor == slug)
        .and_then(|(_, id)| profile(id))
}

/// The profile a configured route binds to, or `None` when the provider is
/// unknown. An unknown *vendor* on a compatible provider falls back to the
/// generic profile; an unknown vendor on a fixed-vendor provider does not,
/// because that combination is a configuration mistake.
pub fn profile_for(provider: &str, vendor: &str) -> Option<&'static RouteProfile> {
    if let Some(exact) = PROFILES
        .iter()
        .find(|profile| profile.provider == provider && profile.vendor == Some(vendor))
    {
        return Some(exact);
    }
    PROFILES
        .iter()
        .find(|profile| profile.provider == provider && profile.vendor.is_none())
}

pub fn profile(id: &str) -> Option<&'static RouteProfile> {
    PROFILES.iter().find(|profile| profile.id == id)
}

pub fn profiles() -> &'static [RouteProfile] {
    PROFILES
}

// -- extension validation -------------------------------------------------

/// Validates operator-declared provider-native options against the profile's
/// allow-list and converts them to the JSON a transport can merge. An unknown
/// key, a wrong type or an out-of-range value is an error: there is no
/// free-form passthrough, so a typo can never reach a vendor unnoticed.
pub fn validate_extensions(
    profile: &RouteProfile,
    declared: &BTreeMap<String, toml::Value>,
) -> Result<BTreeMap<String, serde_json::Value>, String> {
    let mut out = BTreeMap::new();
    for (key, value) in declared {
        let Some(spec) = profile.extensions.iter().find(|spec| spec.key == key) else {
            let known: Vec<&str> = profile.extensions.iter().map(|spec| spec.key).collect();
            return Err(if known.is_empty() {
                format!(
                    "profile `{}` accepts no provider-native extensions, so `{key}` cannot be set",
                    profile.id
                )
            } else {
                format!(
                    "`{key}` is not an extension of profile `{}` (accepted: {})",
                    profile.id,
                    known.join(", ")
                )
            });
        };
        out.insert(key.clone(), convert_extension(spec, value)?);
    }
    Ok(out)
}

fn convert_extension(
    spec: &ExtensionSpec,
    value: &toml::Value,
) -> Result<serde_json::Value, String> {
    let key = spec.key;
    match spec.value {
        ExtensionType::Bool => value
            .as_bool()
            .map(serde_json::Value::Bool)
            .ok_or_else(|| format!("`{key}` must be a boolean")),
        ExtensionType::Integer { min, max } => {
            let number = value
                .as_integer()
                .ok_or_else(|| format!("`{key}` must be an integer"))?;
            if number < min || number > max {
                return Err(format!("`{key}` must be between {min} and {max}"));
            }
            Ok(serde_json::Value::from(number))
        }
        ExtensionType::Number { min, max } => {
            let number = value
                .as_float()
                .or_else(|| value.as_integer().map(|value| value as f64))
                .ok_or_else(|| format!("`{key}` must be a number"))?;
            if number < min || number > max {
                return Err(format!("`{key}` must be between {min} and {max}"));
            }
            serde_json::Number::from_f64(number)
                .map(serde_json::Value::Number)
                .ok_or_else(|| format!("`{key}` must be a finite number"))
        }
        ExtensionType::String => value
            .as_str()
            .map(|text| serde_json::Value::String(text.to_string()))
            .ok_or_else(|| format!("`{key}` must be a string")),
        ExtensionType::Enum { values } => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("`{key}` must be a string"))?;
            if values.contains(&text) {
                Ok(serde_json::Value::String(text.to_string()))
            } else {
                Err(format!("`{key}` must be one of {}", values.join(", ")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalogue_family_binds_to_a_profile_or_states_its_limit() {
        for vendor in crate::commands::ctx::catalogue::vendors() {
            let bound = vendor_profile(vendor.slug).unwrap_or_else(|| {
                panic!(
                    "catalogue vendor `{}` has no route profile; bind it or record a limitation",
                    vendor.slug
                )
            });
            match bound.support {
                Support::Native => {}
                Support::LegacyOnly(reason) => assert!(
                    reason.len() > 40,
                    "`{}` needs a concrete upstream reason",
                    bound.id
                ),
                Support::Planned(_) => panic!(
                    "catalogue vendor `{}` is bound to a planned-only profile",
                    vendor.slug
                ),
            }
        }
    }

    #[test]
    fn broker_profiles_are_legacy_only_and_never_claim_a_vendor_credential() {
        for id in ["copilot-broker", "factory-droid-broker"] {
            let broker = profile(id).unwrap();
            assert!(matches!(broker.support, Support::LegacyOnly(_)));
            assert_eq!(broker.credential, CredentialClass::BrokerSubscription);
            assert!(broker.credential_env.is_empty());
            assert!(!broker.credential.is_optional());
        }
    }

    #[test]
    fn local_profiles_never_require_a_fabricated_key() {
        for id in ["ollama-openai", "lmstudio-openai", "vllm-openai"] {
            let local = profile(id).unwrap();
            assert!(local.credential.is_local(), "{id}");
            assert!(local.credential.is_optional(), "{id}");
            assert!(local.allows_plain_http(), "{id}");
            assert!(
                local
                    .base_url
                    .default_url()
                    .is_some_and(|url| url.starts_with("http://127.0.0.1")),
                "{id} must default to loopback"
            );
        }
        // A remote vendor never gets the plain-http concession.
        assert!(!profile("deepseek-chat").unwrap().allows_plain_http());
    }

    #[test]
    fn an_unknown_compatible_vendor_falls_back_to_the_generic_profile() {
        assert_eq!(
            profile_for("openai-compatible", "some-new-vendor")
                .unwrap()
                .id,
            "openai-chat-generic"
        );
        assert_eq!(
            profile_for("openai-compatible", "deepseek").unwrap().id,
            "deepseek-chat"
        );
        // A fixed-vendor provider has no generic fallback row.
        assert_eq!(
            profile_for("anthropic", "deepseek").map(|profile| profile.id),
            None
        );
        assert_eq!(profile_for("no-such-provider", "deepseek"), None);
    }

    #[test]
    fn extensions_are_an_allow_list_not_a_passthrough() {
        let deepseek = profile("deepseek-chat").unwrap();
        let ok = BTreeMap::from([("temperature".to_string(), toml::Value::Float(0.4))]);
        assert_eq!(
            validate_extensions(deepseek, &ok).unwrap()["temperature"],
            serde_json::json!(0.4)
        );

        let unknown = BTreeMap::from([("logit_bias".to_string(), toml::Value::Integer(1))]);
        let error = validate_extensions(deepseek, &unknown).unwrap_err();
        assert!(error.contains("not an extension of profile `deepseek-chat`"));
        assert!(error.contains("temperature"));

        let mistyped =
            BTreeMap::from([("temperature".to_string(), toml::Value::String("hot".into()))]);
        assert!(
            validate_extensions(deepseek, &mistyped)
                .unwrap_err()
                .contains("must be a number")
        );

        let out_of_range = BTreeMap::from([("temperature".to_string(), toml::Value::Float(9.0))]);
        assert!(
            validate_extensions(deepseek, &out_of_range)
                .unwrap_err()
                .contains("between 0 and 2")
        );

        let closed = profile("anthropic-messages").unwrap();
        assert!(
            validate_extensions(closed, &ok)
                .unwrap_err()
                .contains("accepts no provider-native extensions")
        );
    }

    #[test]
    fn qwen_thinking_switch_is_typed_and_vllm_keeps_its_own_sampling_keys() {
        let qwen = profile("qwen-dashscope-chat").unwrap();
        let enabled = BTreeMap::from([("enable_thinking".to_string(), toml::Value::Boolean(true))]);
        assert_eq!(
            validate_extensions(qwen, &enabled).unwrap()["enable_thinking"],
            serde_json::json!(true)
        );
        // vLLM's top_k is not a Qwen extension and vice versa.
        let top_k = BTreeMap::from([("top_k".to_string(), toml::Value::Integer(20))]);
        assert!(validate_extensions(qwen, &top_k).is_err());
        let vllm = profile("vllm-openai").unwrap();
        assert_eq!(
            validate_extensions(vllm, &top_k).unwrap()["top_k"],
            serde_json::json!(20)
        );
        assert!(validate_extensions(vllm, &enabled).is_err());
    }

    #[test]
    fn profile_ids_are_unique_and_every_row_is_versioned() {
        let mut ids: Vec<&str> = PROFILES.iter().map(|profile| profile.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate profile id");
        assert!(PROFILES.iter().all(|profile| profile.version >= 1));
        assert_eq!(PROFILE_SCHEMA, 1);
    }

    #[test]
    fn exactly_one_generic_fallback_exists_per_provider() {
        let mut generic: Vec<&str> = PROFILES
            .iter()
            .filter(|profile| profile.vendor.is_none())
            .map(|profile| profile.provider)
            .collect();
        generic.sort_unstable();
        let count = generic.len();
        generic.dedup();
        assert_eq!(generic.len(), count, "two fallbacks for one provider");
    }
}
