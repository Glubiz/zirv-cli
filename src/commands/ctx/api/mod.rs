//! `zirv ctx api` (issue #353): the versioned local runtime protocol --
//! its published schema, its reference server, and the CLI wrappers that
//! speak it.
//!
//! Layout mirrors the neighbouring `runtime/` module: [`wire`] is the
//! versioned shapes, [`transport`] is the socket/named-pipe carrier,
//! [`server`] is the one implementation, [`client`] is the minimal client
//! every caller (CLI and test alike) goes through, and [`schema`] renders
//! the contract from the binary's own types.
//!
//! The CLI wrappers stay the normal automation interface, exactly as issue
//! #353 asks: raw protocol access exists for durable subscribers and
//! alternate clients, not because `zirv ctx status` should become a socket
//! call.

pub mod client;
#[cfg(test)]
mod fixtures;
pub mod schema;
pub mod server;
pub mod transport;
pub mod wire;

use std::io::Write;

use clap::{Args, Subcommand};

use super::CtxResult;
use super::config::{EnvLookup, env_from_process};
use super::state::StateDir;
use client::Client;
use server::{ApiServer, RegistrySource, RunningServer};
use wire::Method;

/// How long `zirv ctx api serve` listens by default. A bounded default on
/// purpose: no daemon exists yet (issue #352 owns that), so a `serve` that
/// ran forever would look like one.
const DEFAULT_SERVE_SECONDS: u64 = 60;

#[derive(Debug, Args)]
pub struct ApiArgs {
    #[command(subcommand)]
    pub command: ApiVerb,
}

#[derive(Debug, Subcommand)]
pub enum ApiVerb {
    /// Print the protocol v1 contract: transport, frames, methods, errors
    /// and every published vocabulary, generated from this binary's types.
    Schema {
        /// Emit the generated JSON Schema document instead of the human
        /// rendering.
        #[arg(long)]
        json: bool,
    },
    /// Bind the local runtime endpoint and serve the protocol from an
    /// in-process reference server for a bounded time.
    Serve {
        /// Seconds to listen before exiting.
        #[arg(long, default_value_t = DEFAULT_SERVE_SECONDS)]
        seconds: u64,
    },
    /// Call one protocol method and print its JSON result. Connects to a
    /// live endpoint when there is one, otherwise serves the call from an
    /// in-process reference server for its duration.
    Call {
        /// A method name -- see `zirv ctx api schema`.
        method: String,
        /// Method parameters as a JSON object.
        #[arg(long, default_value = "{}")]
        params: String,
        /// Deduplicate a retried mutation.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

pub fn run(args: &ApiArgs, w: &mut dyn Write) -> CtxResult<i32> {
    let env = env_from_process();
    run_with(args, w, &env)
}

fn run_with(args: &ApiArgs, w: &mut dyn Write, env: EnvLookup<'_>) -> CtxResult<i32> {
    match &args.command {
        ApiVerb::Schema { json } => {
            if *json {
                writeln!(w, "{}", serde_json::to_string_pretty(&schema::json_schema())?)?;
            } else {
                schema::render_human(w)?;
            }
            Ok(0)
        }
        ApiVerb::Serve { seconds } => {
            let state = StateDir::resolve(env)?;
            let endpoint = server::endpoint_for(&state);
            let server = ApiServer::new(Box::new(RegistrySource::new(state)), None);
            let running = RunningServer::start(&endpoint, server)?;
            writeln!(
                w,
                "zirv runtime protocol v{} listening on {} for {seconds}s",
                wire::PROTOCOL_VERSION,
                running.endpoint().display()
            )?;
            std::thread::sleep(std::time::Duration::from_secs(*seconds));
            drop(running);
            Ok(0)
        }
        ApiVerb::Call {
            method,
            params,
            idempotency_key,
        } => {
            let method: Method = method.parse().unwrap_or(Method::Unknown);
            if method == Method::Unknown {
                return Err(format!(
                    "unknown method; `zirv ctx api schema` lists the {} this build serves",
                    wire::METHODS.len()
                )
                .into());
            }
            let params: serde_json::Value = serde_json::from_str(params)
                .map_err(|error| format!("--params must be a JSON object: {error}"))?;
            let state = StateDir::resolve(env)?;
            let endpoint = server::endpoint_for(&state);
            // A live endpoint wins; otherwise this process is the server for
            // the duration of one call. Either way the call travels the real
            // transport rather than short-circuiting into the server object,
            // so the CLI wrapper exercises exactly what an alternate client
            // would.
            let _local = if transport::probe(&endpoint) {
                None
            } else {
                Some(RunningServer::start(
                    &endpoint,
                    ApiServer::new(Box::new(RegistrySource::new(state)), None),
                )?)
            };
            let mut client = Client::connect(&endpoint)?;
            let result = client.call_with_key(method, params, idempotency_key.as_deref())?;
            writeln!(w, "{}", serde_json::to_string_pretty(&result)?)?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_with(state: &std::path::Path) -> HashMap<String, String> {
        HashMap::from([(
            "ZIRV_CTX_STATE_DIR".to_string(),
            state.to_string_lossy().into_owned(),
        )])
    }

    fn render(args: &ApiArgs, env: &HashMap<String, String>) -> (i32, String) {
        let mut out = Vec::new();
        let code = run_with(args, &mut out, &|key| env.get(key).cloned()).expect("run");
        (code, String::from_utf8(out).expect("utf-8"))
    }

    #[test]
    fn schema_prints_the_human_contract_by_default() {
        let env = HashMap::new();
        let (code, text) = render(
            &ApiArgs {
                command: ApiVerb::Schema { json: false },
            },
            &env,
        );
        assert_eq!(code, 0);
        assert!(text.contains("zirv runtime protocol v1"));
        assert!(text.contains("session.snapshot"));
        assert!(text.contains("events.subscribe"));
    }

    #[test]
    fn schema_json_is_a_parseable_document_naming_every_method() {
        let env = HashMap::new();
        let (code, text) = render(
            &ApiArgs {
                command: ApiVerb::Schema { json: true },
            },
            &env,
        );
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let methods = value["methods"].as_array().expect("methods");
        assert_eq!(methods.len(), wire::METHODS.len());
        assert_eq!(value["protocol"], serde_json::json!(1));
    }

    /// The CLI wrapper and the test client exercise the SAME method over the
    /// SAME transport: this call travels the platform socket/pipe into the
    /// in-process reference server it starts for itself.
    #[test]
    fn call_serves_a_method_over_the_real_transport_when_nothing_is_listening() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = env_with(tmp.path());
        let (code, text) = render(
            &ApiArgs {
                command: ApiVerb::Call {
                    method: "server.ping".to_string(),
                    params: "{}".to_string(),
                    idempotency_key: None,
                },
            },
            &env,
        );
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["server"], serde_json::json!("zirv"));
        assert_eq!(value["protocol"], serde_json::json!(1));
    }

    #[test]
    fn call_lists_the_real_session_set_from_the_registry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = env_with(tmp.path());
        let (code, text) = render(
            &ApiArgs {
                command: ApiVerb::Call {
                    method: "session.snapshot".to_string(),
                    params: "{}".to_string(),
                    idempotency_key: None,
                },
            },
            &env,
        );
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(value["sessions"].is_array(), "{value}");
        assert!(value["revision"].is_u64(), "{value}");
    }

    #[test]
    fn call_refuses_a_method_this_build_does_not_serve() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = env_with(tmp.path());
        let mut out = Vec::new();
        let error = run_with(
            &ApiArgs {
                command: ApiVerb::Call {
                    method: "session.levitate".to_string(),
                    params: "{}".to_string(),
                    idempotency_key: None,
                },
            },
            &mut out,
            &|key| env.get(key).cloned(),
        )
        .expect_err("unknown method");
        assert!(error.to_string().contains("unknown method"), "{error}");
    }
}
