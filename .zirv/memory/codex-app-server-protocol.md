## Memory
- Key: codex-app-server-protocol
- Written-by: claude
- Written: 1794981212
- Verified: 1794981212
- Source: explicit

These facts about `codex app-server` were verified against codex-cli 0.163.0 with `codex app-server generate-json-schema`. The protocol is marked [experimental] and its schema is generated per installed binary, so regenerate it after a codex upgrade before trusting a frame shape.

**Wire format.** Frames are newline-delimited JSON-RPC with no `jsonrpc` field.

**Requests.**
- `turn/steer` needs `expectedTurnId`, the id from the `turn/start` acknowledgement. Its input items are `{"type":"text","text":...,"text_elements":[]}`.
- `turn/interrupt` takes `{threadId, turnId}`. It can hang on a starved server, so zirv bounds it at 10 s, the same bound T3 uses.

**Late answers.** A request that timed out can still be answered later. Match each answer to its own request id: a late steer answer once got taken as the answer to the next interrupt.

**Live testing.** Steer and interrupt (#950 slice 7, PR #971) have only run against scripted replay, never a live server.

**Deferred work.**
- The approvals bridge (#950 slice 6) is deferred by supervisor ruling 3d7ef058. It needs an operator-only opt-in for granular approvals, and routing to the verified parent session's dashboard. With no dashboard attached it must decline, never auto-allow.
- ACP (#954) is deferred because no ACP-speaking agent is installed on this machine.
