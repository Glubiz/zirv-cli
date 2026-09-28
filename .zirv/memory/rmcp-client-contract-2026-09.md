## Memory
- Key: rmcp-client-contract-2026-09
- Written-by: codex
- Written: 1790581059
- Verified: 1790581059
- Source: explicit

rmcp 3.3.0 checked 2026-09-28: stock stdio AsyncRwTransport::receive uses unbounded read_until; its codec max length does not apply. Stock HTTP reqwest uses uncapped response.json()/text() for JSON and error bodies; SSE is capped. HTTP reinit_on_expired_session defaults true and retries tool POSTs after 404; zirv forbids replay after start. RequestHandle supports cancellation and timeouts. Safe migration needs custom bounded transports and reinit disabled. See rmcp-v3.3.0 transport sources and issue #795.
