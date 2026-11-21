# MCP 2026-07-28 compliance map

This map links each normative requirement (from `mcp-2026-07-28/` and `mcp-ext-tasks/`) to the code that implements it and the test that covers it. Test names refer to `tests/server.rs` (S), `tests/http.rs` (H), `tests/stdio.rs` (IO), or unit tests in the named module (U).

## Base protocol

| Requirement | Code | Test |
| --- | --- | --- |
| Request ids are strings or integers, never `null` | `mcp/server.rs` `handle_message`, `protocol::valid_request_id` | S `protocol_errors` |
| Every result carries `resultType` | `protocol::result_response` | S `discover_advertises_capabilities_and_caching` |
| Servers SHOULD put `serverInfo` in result `_meta` | `protocol::result_response` | U `protocol::responses` |
| Required `_meta` fields; missing one → `-32602`, HTTP 400 | `protocol::RequestMeta::parse` | S `protocol_errors`, H `json_responses_and_status_codes` |
| Unsupported version → `-32022` with `supported`/`requested`, HTTP 400 | `RequestMeta::parse`, `McpError::unsupported_version` | S `protocol_errors`, H |
| Legacy `initialize` → error naming the supported versions | `handle_message` | IO `invalid_input_is_reported_without_crashing` |
| Must not rely on undeclared client capabilities → `-32021` | `server::check_input_requests`, write tool, `listen`, `task_method` | S `write_tool_requires_confirmation_via_mrtr`, `long_tool_becomes_task` |
| Error codes `-32020..-32022`; never emit `-32002` or `-32042` | `protocol.rs` | — |
| Removed methods (`ping`, `logging/setLevel`, `resources/subscribe`) → `-32601` | `dispatch` | S `protocol_errors` |
| Statelessness: no connection state | everything is per-request `RequestContext` | — |

## Message patterns

| Requirement | Code | Test |
| --- | --- | --- |
| MRTR only on `tools/call`, `prompts/get`, `resources/read` | `call_tool`, `get_prompt` | S |
| `InputRequiredResult` has `inputRequests` and/or `requestState` | `call_tool`, `get_prompt` | S |
| `requestState` is integrity-protected and bound to principal, TTL and request | `mcp/state.rs` | U `state::seal_and_open`, S |
| Unknown `inputResponses` keys are ignored; re-ask when a needed response is missing | tool handlers (re-ask on mismatch) | S |
| Subscriptions: ack first; honoured subset; `subscriptionId` on every message | `subscriptions.rs`, `server::listen` | S `subscriptions_ack_notify_and_close`, H `subscription_stream_lifecycle` |
| Graceful server end → completion result with `subscriptionId` | `Subscription::complete`, `Hub::shutdown` | S `graceful_shutdown…`, H, IO |
| Server sends `notifications/cancelled` only for listen teardown | `Subscription::abort` | — |
| Progress values strictly increase; only with a token | `RequestContext::progress` | S `progress_and_logging_notifications` |
| Cancelled requests get no response | `handle_message` (cancel check), stdio worker | IO `concurrency_cancellation_and_shutdown` |
| HTTP: closing the stream cancels the request | `transport/http.rs` `spawn_disconnect_monitor`, SSE keep-alive | H `subscription_stream_lifecycle` |

## Server features

| Requirement | Code | Test |
| --- | --- | --- |
| `server/discover` (MUST) with cache hints | `server::discover` | S |
| Tools: deterministic order, pagination, `ttlMs`/`cacheScope` | `list_tools`, `paginate` | S `tools_list_is_deterministic_paginated_and_cacheable` |
| Lists may vary by authorization but not by connection | `visible_tools(principal)` | S `partner_tools_and_scopes` |
| Unknown tool → `-32602`; input validation errors → `isError` | `call_tool` | S `negative_results_and_validation_errors_are_tool_errors` |
| `structuredContent` plus JSON text mirror; `outputSchema` conformance | `tools::ok_result`; output schemas only where we control the shape | S |
| `x-mcp-header` constraints | `tools::p_appid`; validated in `tools::tests::registry_is_well_formed` | U |
| Resource not found → `-32602` with `data.uri`; never empty `contents` | `resources::read` | S `resources_read_list_and_templates` |
| `resources/read` and list hints; `subscribe` and `listChanged` | `read_resource`, watcher `watch_tick` | S |
| Prompts: missing argument → `-32602` (or MRTR) | `get_prompt` | S `prompts_embed_resources_and_elicit_missing_args` |
| Completion: ≤100 values, `total`, `hasMore`; unknown ref → `-32602` | `completion.rs`, `server::complete` | S `completion_uses_partner_apps_and_catalog` |
| Logging: only when `logLevel` is set; invalid level → `-32602` | `RequestContext::log`, `RequestMeta::parse` | S, U |
| Invalid cursor → `-32602` | `paginate` | S `protocol_errors` |
| Requests carrying MRTR inputs get no cache hints | MRTR interim results carry none | — |

## Streamable HTTP

| Requirement | Code | Test |
| --- | --- | --- |
| Single POST endpoint; GET/DELETE → 405; no sessions; ignore `Mcp-Session-Id` and `Last-Event-ID` | `HttpServer::route` | H `endpoint_methods…`, `header_body_validation` |
| Notifications → 202, no body | `handle_post` | H `json_responses_and_status_codes` |
| Unknown method → 404 with `-32601` | `McpError::method_not_found` (`http 404`) | H |
| Header validation (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, `Mcp-Param-*`, base64 sentinel, numeric compare) → 400 `-32020` | `HttpServer::validate_headers` | H `header_body_validation` |
| Origin validation → 403 | `route` | H `origin_validation` |
| SSE with `X-Accel-Buffering: no`; keep-alive comments | `http/server.rs::start_sse`, `respond_sse` | H |
| Request-scoped notifications only on their own stream | `SseSink` per request | H `sse_progress_stream` |

## Authorization

| Requirement | Code | Test |
| --- | --- | --- |
| RFC 9728 metadata (root and path-inserted) with `authorization_servers` | `auth::metadata`, `route` | H `authorization_challenges_and_metadata` |
| 401 with `WWW-Authenticate` `resource_metadata` (and `scope`) | `handle_post` | H |
| 403 `insufficient_scope` with the required scope | `handle_post` | H |
| Audience validation (RFC 8707); reject tokens issued for others | `auth::check_claims` | U `auth::jwt_hs256` |
| Never pass tokens upstream | Steam calls use only configured keys | — |
| Scope hierarchy honoured | `context::scopes::implied` | U, H |

## Tasks extension

| Requirement | Code | Test |
| --- | --- | --- |
| Never return `CreateTaskResult` to non-declaring clients | `call_tool` (`ctx.supports_tasks()`) | S `long_tool_becomes_task` |
| Task durably created before returning | `TaskStore::create` before spawning | S |
| `tasks/get` returns the detailed variant with `resultType: complete` | `task_method`, `TaskHandle::to_json(true)` | S |
| `tasks/update`: ignore unknown or satisfied keys; keys unique per task | `TaskHandle::update_inputs`, `request_input` | U `tasks::lifecycle_and_input` |
| `tasks/cancel`: cooperative; terminal states immutable | `request_cancel`, `finish` | U `tasks::cancellation` |
| `failed` only for JSON-RPC errors; `isError` results → `completed` | `run_as_task` | — |
| Non-declaring clients → `-32021` for `tasks/*` and `taskIds` subscriptions | `task_method`, `listen` | S |
| HTTP `Mcp-Name` = `taskId` | `validate_headers` | — |
