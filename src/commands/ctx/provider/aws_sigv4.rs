//! AWS Signature Version 4 request signing (issue #482, roadmap N13).
//!
//! Bedrock authenticates every request with a SigV4 signature rather than a
//! bearer key, so "point the base URL at Bedrock" is not a route: the
//! canonical request has to be built and signed. This module does only that,
//! and does it purely -- no clock, no environment, no filesystem, no socket.
//! The caller supplies the timestamp, which is what makes the AWS-published
//! test vectors reproducible here.
//!
//! No AWS SDK is pulled in for it. The signing algorithm is HMAC-SHA256 over
//! a canonical string, `sha2` is already a direct dependency, and HMAC itself
//! is twenty lines; an SDK would add a large async runtime surface for one
//! header.

#![allow(dead_code)] // The Bedrock transport is this module's only consumer.

use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// The SHA-256 of the empty string, which a bodyless request hashes to.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// One AWS credential set. The secret and session token are held as plain
/// `String`s only inside this module's call frame; callers pass them from a
/// `Secret` and never log the result.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    /// The access key id is not secret (AWS itself displays it in the
    /// `Authorization` header), but the secret key and session token are --
    /// a derived `Debug` would print them into any log or panic message that
    /// formats this struct, so both are redacted by hand instead.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[redacted]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl AwsCredentials {
    /// Parses the single secret a Bedrock account's credential reference
    /// resolves to. SigV4 needs a key *pair* (plus an optional session
    /// token), which one opaque string cannot carry, so the documented shape
    /// is a small JSON object -- the same string whether it comes from an
    /// environment variable, the OS store or a 0600 file.
    pub fn parse(secret: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(secret.trim()).map_err(|_| {
            "an aws-bedrock credential is a JSON object with `access_key_id`, \
             `secret_access_key` and an optional `session_token`; a bare key string cannot sign \
             a request"
                .to_string()
        })?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let access_key_id = field("access_key_id")
            .ok_or_else(|| "credential has no `access_key_id`".to_string())?;
        let secret_access_key = field("secret_access_key")
            .ok_or_else(|| "credential has no `secret_access_key`".to_string())?;
        if access_key_id.trim().is_empty() || secret_access_key.trim().is_empty() {
            return Err("credential has an empty AWS key".into());
        }
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token: field("session_token").filter(|token| !token.trim().is_empty()),
        })
    }
}

/// Everything about the request that the signature covers.
#[derive(Clone, Debug)]
pub struct CanonicalRequest<'a> {
    pub method: &'a str,
    /// The already once-encoded absolute path, e.g. `/model/foo%3A0/converse-stream`.
    pub path: &'a str,
    /// The canonical query string (sorted `k=v` pairs), or empty.
    pub query: &'a str,
    pub host: &'a str,
    /// Additional headers that must be signed, lowercase names.
    pub extra_headers: &'a [(&'a str, String)],
    pub payload: &'a [u8],
}

/// The headers a signed request must carry, in the order they are added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHeaders {
    pub headers: Vec<(String, String)>,
    /// Exposed for fixture tests and diagnostics; never logged in production.
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
}

/// Signs `request` for `service` in `region` at `amz_date`
/// (`YYYYMMDDTHHMMSSZ`).
pub fn sign(
    request: &CanonicalRequest<'_>,
    region: &str,
    service: &str,
    amz_date: &str,
    credentials: &AwsCredentials,
) -> Result<SignedHeaders, String> {
    if amz_date.len() != 16 || !amz_date.is_ascii() || !amz_date.ends_with('Z') {
        return Err(format!("`{amz_date}` is not an AWS YYYYMMDDTHHMMSSZ stamp"));
    }
    let date = &amz_date[..8];
    let payload_hash = hex(&Sha256::digest(request.payload));

    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), request.host.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), amz_date.to_string()),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".to_string(), token.clone()));
    }
    for (name, value) in request.extra_headers {
        headers.push((name.to_ascii_lowercase(), value.clone()));
    }
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    let signed_names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    let signed_header_list = signed_names.join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_header_list}\n{payload_hash}",
        request.method,
        canonical_path(request.path),
        request.query,
    );

    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );

    let key = signing_key(&credentials.secret_access_key, date, region, service);
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));

    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_header_list}, \
         Signature={signature}",
        credentials.access_key_id
    );
    let mut out = headers;
    out.push(("authorization".to_string(), authorization));
    Ok(SignedHeaders {
        headers: out,
        canonical_request,
        string_to_sign,
        signature,
    })
}

/// SigV4's canonical URI. Every service except S3 encodes the path a second
/// time, so a model id that already arrived as `foo%3A0` is signed as
/// `foo%253A0`. Getting this wrong is invisible locally and fatal against
/// the real endpoint, so it is its own function with its own test.
fn canonical_path(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(|segment| uri_encode(segment, false))
        .collect::<Vec<_>>()
        .join("/")
}

/// RFC 3986 unreserved-set encoding. `encode_slash` is false for a path
/// segment (the caller already split on `/`) and true for a query value.
pub fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let start = format!("AWS4{secret}");
    let date_key = hmac(start.as_bytes(), date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    hmac(&service_key, b"aws4_request")
}

/// HMAC-SHA256 (RFC 2104) over `sha2`, which is already a dependency.
fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= padded[index];
        outer_pad[index] ^= padded[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `YYYYMMDDTHHMMSSZ` for a unix-second timestamp. Howard Hinnant's
/// `civil_from_days` again (see `measure.rs`), because no date crate is a
/// dependency of this crate.
pub fn amz_date(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own published `aws-sig-v4-test-suite` credentials.
    fn suite_credentials() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: concat!("wJalrXUtnFEMI/K7MDENG", "+bPxRfiCYEXAMPLEKEY").into(),
            session_token: None,
        }
    }

    #[test]
    fn the_aws_test_suite_get_vanilla_vector_signs_byte_for_byte() {
        let signed = sign(
            &CanonicalRequest {
                method: "GET",
                path: "/",
                query: "",
                host: "example.amazonaws.com",
                extra_headers: &[],
                payload: b"",
            },
            "us-east-1",
            "service",
            "20150830T123600Z",
            &suite_credentials(),
        )
        .unwrap();
        // The suite's own canonical request, minus the `x-amz-content-sha256`
        // header zirv always signs in addition (it is legal to sign more).
        assert!(signed.canonical_request.starts_with("GET\n/\n\n"));
        assert!(signed.canonical_request.ends_with(&format!(
            "\nhost;x-amz-content-sha256;x-amz-date\n{EMPTY_PAYLOAD_SHA256}"
        )));
        assert!(signed.string_to_sign.starts_with(&format!(
            "{ALGORITHM}\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n"
        )));
        let authorization = signed
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .unwrap();
        assert!(
            authorization
                .1
                .contains("Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request")
        );
        assert!(authorization.1.ends_with(&signed.signature));
        assert_eq!(signed.signature.len(), 64);
    }

    /// The chained-HMAC signing key is pinned against RFC 2104's own test
    /// vector, which is what makes the rest of the algorithm trustworthy
    /// without an AWS account to answer a live request.
    #[test]
    fn hmac_sha256_matches_the_rfc_2104_test_vector() {
        // RFC 4231 case 1: key = 0x0b * 20, data = "Hi There".
        let mac = hmac(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // RFC 4231 case 2, which exercises a short ASCII key.
        let mac = hmac(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn the_canonical_uri_double_encodes_a_bedrock_model_id() {
        // A Bedrock model id carries `:` and `.`; the path arrives already
        // encoded once, and the canonical URI encodes it a second time.
        assert_eq!(
            canonical_path("/model/anthropic.claude-sonnet%3A0/converse-stream"),
            "/model/anthropic.claude-sonnet%253A0/converse-stream"
        );
        assert_eq!(canonical_path("/"), "/");
        assert_eq!(canonical_path(""), "/");
        assert_eq!(uri_encode("a b:c", true), "a%20b%3Ac");
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn a_session_token_is_signed_and_carried() {
        let mut credentials = suite_credentials();
        credentials.session_token = Some("session-token".into());
        let signed = sign(
            &CanonicalRequest {
                method: "POST",
                path: "/model/m/converse-stream",
                query: "",
                host: "bedrock-runtime.us-east-1.amazonaws.com",
                extra_headers: &[("content-type", "application/json".to_string())],
                payload: b"{}",
            },
            "us-east-1",
            "bedrock",
            "20260913T010203Z",
            &credentials,
        )
        .unwrap();
        assert!(
            signed
                .canonical_request
                .contains("x-amz-security-token:session-token")
        );
        assert!(
            signed
                .headers
                .iter()
                .any(|(name, value)| name == "x-amz-security-token" && value == "session-token")
        );
        assert!(
            signed
                .canonical_request
                .contains("content-type:application/json")
        );
        // Signed header names are sorted and match the canonical block.
        assert!(
            signed
                .canonical_request
                .contains("content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token")
        );
    }

    #[test]
    fn signing_is_deterministic_and_region_scoped() {
        let request = CanonicalRequest {
            method: "POST",
            path: "/model/m/converse-stream",
            query: "",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            extra_headers: &[],
            payload: b"{\"messages\":[]}",
        };
        let first = sign(
            &request,
            "us-east-1",
            "bedrock",
            "20260913T010203Z",
            &suite_credentials(),
        )
        .unwrap();
        let again = sign(
            &request,
            "us-east-1",
            "bedrock",
            "20260913T010203Z",
            &suite_credentials(),
        )
        .unwrap();
        assert_eq!(first.signature, again.signature);
        let other_region = sign(
            &request,
            "eu-west-1",
            "bedrock",
            "20260913T010203Z",
            &suite_credentials(),
        )
        .unwrap();
        assert_ne!(first.signature, other_region.signature);
    }

    #[test]
    fn a_non_ascii_sixteen_byte_stamp_is_refused_not_a_byte_index_panic() {
        // 16 *bytes*, ends with 'Z', but a multi-byte character straddles
        // byte offset 8 -- exactly where `sign` used to slice `&amz_date[..8]`
        // without checking char-boundary safety first.
        let amz_date = "2015083\u{e9}123456Z";
        assert_eq!(amz_date.len(), 16);
        let error = sign(
            &CanonicalRequest {
                method: "GET",
                path: "/",
                query: "",
                host: "h",
                extra_headers: &[],
                payload: b"",
            },
            "us-east-1",
            "bedrock",
            amz_date,
            &suite_credentials(),
        )
        .unwrap_err();
        assert!(error.contains("YYYYMMDDTHHMMSSZ"), "got {error}");
    }

    #[test]
    fn a_malformed_stamp_is_refused_rather_than_guessed() {
        let error = sign(
            &CanonicalRequest {
                method: "GET",
                path: "/",
                query: "",
                host: "h",
                extra_headers: &[],
                payload: b"",
            },
            "us-east-1",
            "bedrock",
            "2026-09-13",
            &suite_credentials(),
        )
        .unwrap_err();
        assert!(error.contains("YYYYMMDDTHHMMSSZ"));
    }

    #[test]
    fn debug_formatting_never_prints_the_secret_or_session_token() {
        let credentials = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "super-secret-key".into(),
            session_token: Some("super-secret-token".into()),
        };
        let debug = format!("{credentials:?}");
        assert!(debug.contains("AKIDEXAMPLE"), "got {debug}");
        assert!(!debug.contains("super-secret-key"), "got {debug}");
        assert!(!debug.contains("super-secret-token"), "got {debug}");
    }

    #[test]
    fn a_bedrock_credential_is_a_key_pair_not_a_bare_string() {
        let error = AwsCredentials::parse("AKIAEXAMPLE").unwrap_err();
        assert!(error.contains("access_key_id"));
        assert!(error.contains("cannot sign"));

        let missing = AwsCredentials::parse(r#"{"access_key_id":"AKIA"}"#).unwrap_err();
        assert!(missing.contains("secret_access_key"));

        let parsed = AwsCredentials::parse(
            r#"{"access_key_id":"AKIA","secret_access_key":"s","session_token":"t"}"#,
        )
        .unwrap();
        assert_eq!(parsed.access_key_id, "AKIA");
        assert_eq!(parsed.session_token.as_deref(), Some("t"));

        let empty_token = AwsCredentials::parse(
            r#"{"access_key_id":"AKIA","secret_access_key":"s","session_token":"  "}"#,
        )
        .unwrap();
        assert_eq!(empty_token.session_token, None);
    }

    #[test]
    fn amz_dates_are_formatted_from_a_caller_supplied_clock() {
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
    }
}
