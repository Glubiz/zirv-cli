## Memory
- Key: codex-app-server-protocol
- Written-by: claude
- Written: 1795175100
- Verified: 1795175100
- Source: explicit

These facts about `codex app-server` were verified against codex-cli 0.163.0 with `codex app-server generate-json-schema`. The protocol is marked [experimental] and its schema is generated per installed binary, so regenerate it after a codex upgrade before trusting a frame shape.

**Wire format.** Frames are newline-delimited JSON-RPC with no `jsonrpc` field.

**Requests.**
- `turn/steer` needs `expectedTurnId`, the id from the `turn/start` acknowledgement. Its input items are `{"type":"text","text":...,"text_elements":[]}`.
- `turn/interrupt` takes `{threadId, turnId}`. It can hang on a starved server, so zirv bounds it at 10 s, the same bound T3 uses.

**Late answers.** A request that timed out can still be answered later. Match each answer to its own request id: a late steer answer once got taken as the answer to the next interrupt.

**Live testing.** Steer and interrupt (#950 slice 7, PR #971) and the approvals bridge (#950 slice 6, PR #978) have only run against scripted replay, never a live server.

**Approvals (#950 slice 6).**
- `-a on-request` means the model decides when to ask (`codex --help`, 0.163.0).
- The schema does not say what the `granular` policy booleans mean, so do not use them without a live probe.
- `item/fileChange/requestApproval` params carry no diff, so zirv declines them.
- `serverRequest/resolved {requestId}` means codex no longer waits for that answer.
- Unverified: whether codex reuses a pending request id, and how it reacts to repeated declines or to an answer after `serverRequest/resolved`. zirv fails closed on each.

**Deferred work.** ACP (#954) is deferred because no ACP-speaking agent is installed on this machine.
