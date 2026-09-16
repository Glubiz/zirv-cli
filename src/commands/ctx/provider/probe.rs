use std::time::Duration;

use super::credential::Credential;
use super::profiles::RouteProfile;
use super::{AuthScheme, Protocol, ProviderSpec};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeResult {
    Unreachable(String),
    Http { status: u16, model_ids: Vec<String> },
}

pub trait Probe {
    fn models(
        &self,
        endpoint_url: &str,
        spec: &ProviderSpec,
        credential: Option<&Credential>,
    ) -> ProbeResult;
}

pub(crate) fn is_plaintext_non_loopback(endpoint_url: &str) -> bool {
    let Some(host) = plain_http_host(endpoint_url) else {
        return false;
    };
    !host.eq_ignore_ascii_case("localhost") && host != "127.0.0.1" && host != "::1"
}

/// Whether a plain-HTTP URL points at a host a self-hosted model runtime may
/// legitimately live on: loopback, an RFC 1918 private range, IPv4
/// link-local, or a unique-local/link-local IPv6 address. A name that is not
/// a literal address is never assumed local -- `models.example.com`
/// resolving to a private address today says nothing about tomorrow. This is
/// strictly weaker than [`is_plaintext_non_loopback`], which still governs
/// whether a *credential* may cross the wire.
pub(crate) fn is_local_http_host(endpoint_url: &str) -> bool {
    let Some(host) = plain_http_host(endpoint_url) else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

fn plain_http_host(endpoint_url: &str) -> Option<&str> {
    let rest = endpoint_url.strip_prefix("http://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    Some(if let Some(bracketed) = host_and_port.strip_prefix('[') {
        bracketed
            .split_once(']')
            .map_or(bracketed, |(host, _)| host)
    } else if host_and_port.eq_ignore_ascii_case("::1") {
        host_and_port
    } else {
        host_and_port
            .split_once(':')
            .map_or(host_and_port, |(host, _)| host)
    })
}

pub struct HttpProbe {
    agent: ureq::Agent,
}

impl Default for HttpProbe {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(10)))
                .timeout_connect(Some(Duration::from_secs(10)))
                .build()
                .into(),
        }
    }
}

impl Probe for HttpProbe {
    fn models(
        &self,
        endpoint_url: &str,
        spec: &ProviderSpec,
        credential: Option<&Credential>,
    ) -> ProbeResult {
        let mut request = self.agent.get(endpoint_url);
        if let Some(credential) = credential {
            match spec.auth {
                AuthScheme::Header {
                    name,
                    scheme,
                    extra_headers,
                } => {
                    let value = scheme.map_or_else(
                        || credential.secret.expose().to_string(),
                        |scheme| format!("{scheme} {}", credential.secret.expose()),
                    );
                    request = request.header(name, value);
                    for (name, value) in extra_headers {
                        request = request.header(*name, *value);
                    }
                }
                AuthScheme::Query { name } => {
                    request = request.query(name, credential.secret.expose());
                }
            }
        } else if let AuthScheme::Header { extra_headers, .. } = spec.auth {
            for (name, value) in extra_headers {
                request = request.header(*name, *value);
            }
        }
        match request.call() {
            Ok(mut response) => {
                let status = response.status().as_u16();
                if status != 200 {
                    return ProbeResult::Http {
                        status,
                        model_ids: Vec::new(),
                    };
                }
                let body = match response.body_mut().read_to_string() {
                    Ok(body) => body,
                    Err(error) => return ProbeResult::Unreachable(error.to_string()),
                };
                ProbeResult::Http {
                    status,
                    model_ids: parse_model_ids(spec.protocol, &body),
                }
            }
            Err(ureq::Error::StatusCode(status)) => ProbeResult::Http {
                status,
                model_ids: Vec::new(),
            },
            Err(error) => ProbeResult::Unreachable(error.to_string()),
        }
    }
}

pub(crate) fn models_url(
    base_url: &str,
    spec: &ProviderSpec,
    profile: Option<&RouteProfile>,
) -> Option<String> {
    let path = if spec.protocol == Protocol::OpenAiChatCompatible {
        match profile.and_then(|profile| profile.path.strip_suffix("/chat/completions")) {
            Some(prefix) => format!("{prefix}/models"),
            None => spec.models_list_path?.to_string(),
        }
    } else {
        spec.models_list_path?.to_string()
    };
    Some(join_url_path(base_url, &path))
}

pub(crate) fn join_url_path(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let first = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    let duplicate = format!("/{first}");
    let path = if base.ends_with(&duplicate) {
        path.strip_prefix(&duplicate).unwrap_or(path)
    } else {
        path
    };
    format!("{base}{path}")
}

fn parse_model_ids(protocol: Protocol, body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let key = if protocol == Protocol::GoogleGenerativeAi {
        "models"
    } else {
        "data"
    };
    value
        .get(key)
        .and_then(|models| models.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let field = if protocol == Protocol::GoogleGenerativeAi {
                "name"
            } else {
                "id"
            };
            model
                .get(field)
                .and_then(|id| id.as_str())
                .map(|id| id.strip_prefix("models/").unwrap_or(id).to_string())
        })
        .collect()
}

#[cfg(test)]
#[derive(Clone)]
pub struct FakeProbe {
    result: ProbeResult,
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>>,
}

#[cfg(test)]
impl FakeProbe {
    pub fn new(result: ProbeResult) -> Self {
        Self {
            result,
            calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn calls(&self) -> Vec<(String, bool)> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Probe for FakeProbe {
    fn models(
        &self,
        endpoint_url: &str,
        _: &ProviderSpec,
        credential: Option<&Credential>,
    ) -> ProbeResult {
        self.calls
            .lock()
            .unwrap()
            .push((endpoint_url.to_string(), credential.is_some()));
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_parsers_keep_only_ids() {
        let openai = parse_model_ids(
            Protocol::OpenAiResponses,
            r#"{"data":[{"id":"gpt-5"},{"name":"ignored"}],"secret":"no"}"#,
        );
        assert_eq!(openai, ["gpt-5"]);
        let google = parse_model_ids(
            Protocol::GoogleGenerativeAi,
            r#"{"models":[{"name":"models/gemini-3"}]}"#,
        );
        assert_eq!(google, ["gemini-3"]);
    }

    #[test]
    fn cross_host_redirect_never_forwards_provider_secret_headers() {
        // Issue #558.
        let probe = HttpProbe::default();
        assert_eq!(probe.agent.config().max_redirects(), 0);
    }
}
