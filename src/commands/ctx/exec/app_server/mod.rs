//! Codex `app-server` transport for headless workers (#950, slices 1-3).
//!
//! Newline-delimited JSON-RPC over the child's stdio. `codec` is the pure
//! line codec and typed frames, `replay` the scripted fake-server harness,
//! `probe` the `--listen` probe, handshake and fall-back-to-`codex exec`
//! decision. Unix-only v1; nothing calls this yet.

pub mod codec;
pub mod probe;
pub mod replay;
