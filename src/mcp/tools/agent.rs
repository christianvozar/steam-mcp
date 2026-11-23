//! Tools for agents and the operators who run them (ADR 0018): batched
//! read calls, per-principal usage metering and the write audit trail.

use super::*;
use crate::mcp::context::scopes;

fn batch_call(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let cfg = env.cfg();
    let calls = args
        .get("calls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if calls.is_empty() {
        return tool_error("'calls' must list at least one {tool, arguments} entry.");
    }
    if calls.len() > cfg.batch_max_calls {
        return tool_error(format!(
            "A batch holds at most {} calls (TWITCH_MCP_BATCH_MAX_CALLS / STEAM_MCP_BATCH_MAX_CALLS); split it.",
            cfg.batch_max_calls
        ));
    }
    env.core.agents.note(env.principal(), "batch");
    let workers = cfg.batch_parallelism.min(calls.len()).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Value>> = (0..calls.len())
        .map(|_| std::sync::Mutex::new(Value::Null))
        .collect();
    let done = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= calls.len() {
                    break;
                }
                let c = &calls[i];
                let item = if env.cancelled() {
                    obj! { "tool" => c["tool"].clone(), "ok" => false, "error" => obj! { "code" => -32800, "message" => "Request cancelled" } }
                } else {
                    match c["tool"].as_str() {
                        Some(name) => env.core.execute_read(env.ctx, name, &c["arguments"]),
                        None => obj! { "ok" => false, "error" => obj! { "code" => -32602, "message" => "each call needs a string 'tool'" } },
                    }
                };
                *results[i].lock().unwrap_or_else(|e| e.into_inner()) = item;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                env.progress(n as f64, Some(calls.len() as f64), "batch");
            });
        }
    });
    let mut items: Vec<Value> = results
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect();
    for (i, it) in items.iter_mut().enumerate() {
        it.set("index", i as u64);
        if let Some(id) = calls[i].get("id") {
            it.set("id", id.clone());
        }
    }
    let ok = items
        .iter()
        .filter(|i| i["ok"].as_bool() == Some(true))
        .count();
    let failed: Vec<String> = items
        .iter()
        .filter(|i| i["ok"].as_bool() != Some(true))
        .take(5)
        .map(|i| {
            format!(
                "#{} {}: {}",
                i["index"],
                i["tool"].as_str().unwrap_or("?"),
                i.at(&["error", "message"])
                    .as_str()
                    .or(i["text"].as_str())
                    .unwrap_or("error")
            )
        })
        .collect();
    let text = if failed.is_empty() {
        format!("All {ok} call(s) succeeded.")
    } else {
        format!(
            "{ok} of {} call(s) succeeded. Failures: {}.",
            items.len(),
            failed.join("; ")
        )
    };
    ok_result(
        text,
        obj! { "count" => items.len() as u64, "succeeded" => ok as u64, "results" => items },
        &[],
    )
}

fn usage_stats(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let cfg = env.cfg();
    let me = env.principal().to_string();
    let who = arg_str(args, "principal")
        .unwrap_or(me.as_str())
        .to_string();
    let admin = env.ctx.principal.has_scope(scopes::ADMIN);
    if who != me && !admin {
        return tool_error(format!(
            "Reading another principal's usage requires scope {}.",
            scopes::ADMIN
        ));
    }
    let rows = env
        .core
        .agents
        .usage(&cfg, if who == "*" { None } else { Some(who.as_str()) });
    let text = if who == "*" {
        let top: Vec<String> = rows
            .iter()
            .take(10)
            .map(|r| {
                format!(
                    "{} {} calls ({} upstream, {} limited)",
                    r["principal"].as_str().unwrap_or("?"),
                    r["toolCalls"],
                    r["upstreamCalls"],
                    r["rateLimited"]
                )
            })
            .collect();
        format!(
            "{} principal(s). Busiest: {}.",
            rows.len(),
            if top.is_empty() {
                "none".into()
            } else {
                top.join("; ")
            }
        )
    } else {
        match rows.first() {
            Some(r) => format!(
                "{}: {} tool calls ({} today), {} errors, {} upstream fetches ({} today), {} rate-limited, {} writes ({} policy-approved), {} in flight.",
                who,
                r["toolCalls"],
                r["today"]["toolCalls"],
                r["toolErrors"],
                r["upstreamCalls"],
                r["today"]["upstreamCalls"],
                r["rateLimited"],
                r["writes"],
                r["policyApprovedWrites"],
                r["inflight"]
            ),
            None => format!("No usage recorded for {who}."),
        }
    };
    ok_result(text, obj! { "principals" => rows }, &[])
}

fn audit_log(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let me = env.principal().to_string();
    let admin = env.ctx.principal.has_scope(scopes::ADMIN);
    let who = match arg_str(args, "principal") {
        Some("*") if admin => None,
        Some(p) if admin || p == me => Some(p.to_string()),
        Some(_) => {
            return tool_error(format!(
                "Reading another principal's audit trail requires scope {}.",
                scopes::ADMIN
            ));
        }
        None if admin => None,
        None => Some(me),
    };
    let limit = arg_u64(args, "limit").unwrap_or(50).clamp(1, 2000) as usize;
    let entries = env.core.agents.audit_entries(who.as_deref(), limit);
    let lines: Vec<String> = entries
        .iter()
        .take(5)
        .map(|e| {
            format!(
                "{} {} {} ({})",
                e["at"].as_str().unwrap_or("?"),
                e["principal"].as_str().unwrap_or("?"),
                e["action"].as_str().unwrap_or("?"),
                e["approvedBy"].as_str().unwrap_or("?")
            )
        })
        .collect();
    ok_result(
        if entries.is_empty() {
            "No writes recorded since the server started.".into()
        } else {
            format!(
                "{} write(s), newest first: {}.",
                entries.len(),
                lines.join("; ")
            )
        },
        obj! { "entries" => entries },
        &[],
    )
}

pub fn tools() -> Vec<ToolDef> {
    let call = obj! {
        "type" => "object",
        "properties" => obj! {
            "tool" => obj! { "type" => "string", "minLength" => 1, "description" => "A read-only tool name." },
            "arguments" => obj! { "type" => "object", "description" => "That tool's arguments." },
            "id" => obj! { "type" => ["string", "integer"], "description" => "Optional caller ID echoed back in the result." },
        },
        "required" => crate::arr!["tool"],
        "additionalProperties" => false,
    };
    vec![
        ToolDef {
            name: "batch_call",
            title: "Batch Read Calls",
            description: "Runs several read-only tool calls in one request, in parallel, and returns every result in order (each with ok, text, structuredContent and cache provenance, or an error). Each call is checked, validated, rate-limited and cached exactly like a normal call. Writes, exports and nested batches are refused.".into(),
            input_schema: object_schema(vec![("calls", obj! { "type" => "array", "items" => call, "minItems" => 1, "maxItems" => 200 })], &["calls"]),
            output_schema: Some(obj! { "type" => "object", "properties" => obj! { "count" => obj! { "type" => "integer" }, "succeeded" => obj! { "type" => "integer" }, "results" => obj! { "type" => "array" } }, "required" => crate::arr!["count", "succeeded", "results"] }),
            hints: READ,
            scope: scopes::PUBLIC,
            key: Default::default(),
            task: false,
            write: false,
            handler: batch_call,
        },
        ToolDef {
            name: "usage_stats",
            title: "Usage and Quotas",
            description: "Tool calls, errors, upstream fetches, rate-limit rejections, writes and the limits that apply, for the calling principal (or, with the admin scope, any principal or '*' for all, busiest first).".into(),
            input_schema: object_schema(vec![("principal", p_str("Principal ID, or '*' for everyone (admin only). Defaults to you."))], &[]),
            output_schema: Some(obj! { "type" => "object", "properties" => obj! { "principals" => obj! { "type" => "array" } }, "required" => crate::arr!["principals"] }),
            hints: LOCAL,
            scope: scopes::PUBLIC,
            key: Default::default(),
            task: false,
            write: false,
            handler: usage_stats,
        },
        ToolDef {
            name: "audit_log",
            title: "Write Audit Log",
            description: "Recent state-changing actions with who made them and how they were approved (user confirmation or a named write-policy rule), newest first. Your own entries, or everyone's with the admin scope.".into(),
            input_schema: object_schema(vec![("principal", p_str("Principal ID, or '*' (admin only).")), ("limit", p_int("Entries.", 1, 2000, Some(50)))], &[]),
            output_schema: Some(obj! { "type" => "object", "properties" => obj! { "entries" => obj! { "type" => "array" } }, "required" => crate::arr!["entries"] }),
            hints: LOCAL,
            scope: scopes::PUBLIC,
            key: Default::default(),
            task: false,
            write: false,
            handler: audit_log,
        },
    ]
}
