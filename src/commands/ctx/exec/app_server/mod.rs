//! Codex `app-server` transport for headless workers (#950).
//!
//! Newline-delimited JSON-RPC over the child's stdio. `codec` is the pure
//! line codec and typed frames, `replay` the scripted fake-server harness,
//! `probe` the `--listen` probe, handshake and fall-back-to-`codex exec`
//! decision, `translate` the exec launch re-expressed for the server, and
//! `turn` the one headless turn exec supervises. Unix-only; only an
//! operator's `[headless] codex_app_server` opts a first codex launch in.

pub mod codec;
pub mod probe;
#[cfg(test)]
pub mod replay;
pub mod translate;
pub mod turn;
