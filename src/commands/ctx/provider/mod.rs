//! Native provider identities and the built-in provider registry (issue #471).

pub mod adapter;
pub mod anthropic;
pub mod aws_sigv4;
pub mod bedrock;
pub mod capability;
pub mod config;
pub mod credential;
pub mod google;
pub mod inventory;
pub mod openai;
pub mod openai_chat;
pub mod probe;
pub mod profiles;
pub mod transport;

use serde::{Deserialize, Serialize};

/// Provider-owned continuation material is persisted verbatim but never
/// printed through `Debug`; signatures and redacted-thinking payloads are
/// protocol data, not diagnostics.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpaqueProviderData(serde_json::Value);

impl OpaqueProviderData {
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &serde_json::Value {
        &self.0
    }
}

impl std::fmt::Debug for OpaqueProviderData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[opaque provider data]")
    }
}

macro_rules! slug_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                if valid_slug(&value) {
                    Ok(Self(value))
                } else {
                    Err(format!(
                        "invalid {} {:?}; expected [a-z0-9][a-z0-9._-]{{0,63}}",
                        stringify!($name),
                        value
                    ))
                }
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && value.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

slug_id!(ProviderId);
slug_id!(EndpointId);
slug_id!(AccountId);
slug_id!(BillingPoolId);
slug_id!(RouteId);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId {
    pub vendor: String,
    pub id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingClass {
    #[default]
    Api,
    Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    AnthropicMessages,
    OpenAiResponses,
    OpenAiChatCompatible,
    GoogleGenerativeAi,
    GoogleVertex,
    AwsBedrock,
    AzureOpenAiChat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AuthScheme {
    Header {
        name: &'static str,
        scheme: Option<&'static str>,
        extra_headers: &'static [(&'static str, &'static str)],
    },
    #[allow(dead_code)] // N12's Google transport may need query-key authentication.
    Query { name: &'static str },
}

/// How a route is served today.
///
/// `Planned` is zirv's own gap -- an adapter nobody has written yet.
/// `LegacyOnly` is an *upstream* one: the route exists, but there is no
/// documented, separately authorized direct API a third-party client may
/// authenticate against, so the coding-harness backend stays the only honest
/// way to spend it. The two are never interchangeable: conflating them would
/// let an unimplemented adapter masquerade as an entitlement problem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Support {
    Native,
    // Kept even while no built-in row uses it: the distinction between "zirv
    // has not written this adapter" and "upstream does not offer one" is the
    // contract, and collapsing the enum to whatever is unimplemented today
    // would erase it.
    #[allow(dead_code)]
    Planned(&'static str),
    #[serde(rename = "legacy-only")]
    LegacyOnly(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub protocol: Protocol,
    pub vendor: Option<&'static str>,
    pub default_base_url: Option<&'static str>,
    pub default_credential_env: &'static [&'static str],
    pub auth: AuthScheme,
    pub models_list_path: Option<&'static str>,
    pub support: Support,
    pub entitlement_note: &'static str,
}

const SUBSCRIPTION_NOTE: &str = "Claude.ai and ChatGPT subscription logins are not API access; harness login tokens are never reused.";

pub static PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        protocol: Protocol::AnthropicMessages,
        vendor: Some("anthropic"),
        default_base_url: Some("https://api.anthropic.com"),
        default_credential_env: &["ANTHROPIC_API_KEY"],
        auth: AuthScheme::Header {
            name: "x-api-key",
            scheme: None,
            extra_headers: &[("anthropic-version", "2023-06-01")],
        },
        models_list_path: Some("/v1/models"),
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
    ProviderSpec {
        id: "openai",
        protocol: Protocol::OpenAiResponses,
        vendor: Some("openai"),
        default_base_url: Some("https://api.openai.com"),
        default_credential_env: &["OPENAI_API_KEY"],
        auth: AuthScheme::Header {
            name: "Authorization",
            scheme: Some("Bearer"),
            extra_headers: &[],
        },
        models_list_path: Some("/v1/models"),
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
    ProviderSpec {
        id: "google",
        protocol: Protocol::GoogleGenerativeAi,
        vendor: Some("google"),
        default_base_url: Some("https://generativelanguage.googleapis.com"),
        default_credential_env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        auth: AuthScheme::Header {
            name: "x-goog-api-key",
            scheme: None,
            extra_headers: &[],
        },
        models_list_path: Some("/v1beta/models"),
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
    ProviderSpec {
        id: "openai-compatible",
        protocol: Protocol::OpenAiChatCompatible,
        vendor: None,
        default_base_url: None,
        default_credential_env: &[],
        auth: AuthScheme::Header {
            name: "Authorization",
            scheme: Some("Bearer"),
            extra_headers: &[],
        },
        models_list_path: Some("/v1/models"),
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
    ProviderSpec {
        id: "google-vertex",
        protocol: Protocol::GoogleVertex,
        vendor: Some("google"),
        default_base_url: None,
        default_credential_env: &[],
        auth: AuthScheme::Header {
            name: "Authorization",
            scheme: Some("Bearer"),
            extra_headers: &[],
        },
        models_list_path: None,
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
    ProviderSpec {
        id: "aws-bedrock",
        protocol: Protocol::AwsBedrock,
        // Bedrock serves several vendors' models behind one account, so the
        // endpoint declares which one it is pointed at -- exactly as an
        // openai-compatible endpoint does.
        vendor: None,
        default_base_url: None,
        default_credential_env: &[],
        auth: AuthScheme::Header {
            name: "Authorization",
            scheme: None,
            extra_headers: &[],
        },
        models_list_path: None,
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
    ProviderSpec {
        id: "azure-openai",
        protocol: Protocol::AzureOpenAiChat,
        vendor: Some("openai"),
        default_base_url: None,
        default_credential_env: &["AZURE_OPENAI_API_KEY"],
        auth: AuthScheme::Header {
            name: "api-key",
            scheme: None,
            extra_headers: &[],
        },
        // The deployment listing is an ARM/management call, not a data-plane
        // one; there is no `/v1/models` to probe here.
        models_list_path: None,
        support: Support::Native,
        entitlement_note: SUBSCRIPTION_NOTE,
    },
];

/// A one-request loopback HTTP server, shared by the direct transports'
/// tests so each of them proves its real wire request instead of trusting an
/// encoder unit test.
#[cfg(test)]
pub(crate) mod testhttp {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Answers exactly one request with `status` and `body`, handing the
    /// caller the raw request text it received.
    pub(crate) fn one_shot_server(
        status: u16,
        body: &'static str,
        content_type: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        one_shot_bytes_server(status, body.as_bytes().to_vec(), content_type)
    }

    /// The same, for a body that is not valid UTF-8: AWS event-stream frames
    /// carry binary length and CRC fields.
    pub(crate) fn one_shot_bytes_server(
        status: u16,
        body: Vec<u8>,
        content_type: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let reason = if status == 200 { "OK" } else { "Error" };
            let content_type = if status == 200 {
                content_type
            } else {
                "application/json"
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        });
        (format!("http://{address}"), receiver)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut expected = None;
        while let Ok(count) = stream.read(&mut buffer) {
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if expected.is_none()
                && let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                expected = Some(end + 4 + length);
            }
            if expected.is_some_and(|expected| bytes.len() >= expected) {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

pub fn provider(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|spec| spec.id == id)
}

pub fn providers() -> &'static [ProviderSpec] {
    PROVIDERS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_slugs_are_bounded_lowercase_names() {
        assert_eq!(RouteId::new("work-sonnet").unwrap().as_ref(), "work-sonnet");
        for invalid in ["", "Upper", "-leading", "has space", &"a".repeat(65)] {
            assert!(RouteId::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
