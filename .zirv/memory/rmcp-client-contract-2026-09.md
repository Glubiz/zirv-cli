## Memory
- Key: rmcp-client-contract-2026-09
- Written-by: claude
- Written: 1794289067
- Verified: 1794289067
- Source: explicit

rmcp 3.9.0 (pinned =3.9.0 since 4.69.0, for RUSTSEC-2026-0457/0461/0462) checked 2026-11-09: stock stdio AsyncRwTransport::receive now bounds each line at DEFAULT_MAX_LINE_LENGTH = 16 MiB (src/transport/async_rw.rs:58), replacing 3.3.0's unbounded read_until. Per RUSTSEC-2026-0462 the reqwest/unix-socket Streamable HTTP clients cap JSON bodies at 16 MiB and error bodies at 64 KiB from 3.9.0; SSE events were already capped. HTTP reinit_on_expired_session still defaults true (streamable_http_client.rs:2245) and retries POSTs after 404; zirv forbids replay after start, so any HTTP client use must disable it. zirv enables only server, client, transport-io and transport-async-rw (no HTTP transport). ListToolsResult's ttl_ms/cache_scope (src/model.rs:1487, :1489) default to 0 and Private, so zirv's explicit with_ttl_ms/with_cache_scope in ctx/mcp.rs list_tools remains required. model::ServerInfo is deprecated (since 3.7) in favour of ServerConfig.
