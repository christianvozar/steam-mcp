//! Server administration and cross-cutting tools: cache inspection and
//! invalidation, a guarded generic Web API passthrough, report export (MRTR
//! roots) and LLM-assisted review summaries (MRTR sampling).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::*;
use crate::try_steam;
use crate::mcp::context::{Transport, scopes};
use crate::steam::{Call, Host, KeyKind};
use crate::util::encoding::pct_decode;

fn cache_stats(env: &ToolEnv, _args: &Map) -> ToolOutcome {
    let cache = env.steam().cache.stats_json();
    let upstream = env.steam().stats_json();
    let ns: Map = env.steam().cache.key_namespaces().into_iter().map(|(k, v)| (k, Value::from(v))).collect();
    let text = format!(
        "Cache: {} entries (~{} KiB), hit ratio {:.1}%, {} upstream calls ({} errors), {} of {} daily budget used.",
        cache["entries"],
        cache["approxBytes"].as_u64().unwrap_or(0) / 1024,
        cache["hitRatio"].as_f64().unwrap_or(0.0) * 100.0,
        upstream["upstreamCalls"],
        upstream["upstreamErrors"],
        upstream["dailyBudgetUsed"],
        upstream["dailyBudget"]
    );
    let ttl: Map = env.cfg().ttl.iter().map(|(k, (t, s))| (k.clone(), obj! { "ttlSeconds" => *t, "staleSeconds" => *s })).collect();
    ok_result(text, obj! { "cache" => cache, "upstream" => upstream, "namespaces" => Value::Object(ns), "ttlClasses" => Value::Object(ttl) }, &[])
}

fn cache_invalidate(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let cache = &env.steam().cache;
    let scope = arg_str(args, "scope").unwrap_or("app");
    let removed = match scope {
        "all" => cache.clear(),
        "app" => {
            let Some(appid) = arg_u64(args, "appid") else { return tool_error("scope=app requires 'appid'.") };
            let needles = [format!("appid={appid}"), format!("appids={appid}"), format!("gameid={appid}"), format!("/appreviews/{appid}?")];
            cache.invalidate_where(|k| needles.iter().any(|n| k.contains(n.as_str())))
        }
        "player" => {
            let Some(id) = arg_str(args, "steamid") else { return tool_error("scope=player requires 'steamid'.") };
            let id = id.to_string();
            cache.invalidate_where(|k| k.contains(&id))
        }
        "prefix" => {
            let Some(p) = arg_str(args, "prefix") else { return tool_error("scope=prefix requires 'prefix'.") };
            let p = p.to_string();
            cache.invalidate_where(|k| k.starts_with(&p))
        }
        "catalog" => cache.invalidate_where(|k| k.starts_with("catalog:")),
        other => return tool_error(format!("Unknown scope '{other}'.")),
    };
    crate::log_info!("audit", "cache invalidated", scope = scope, removed = removed, principal = env.ctx.principal.id);
    ok_result(format!("Invalidated {removed} cache entr{} (scope {scope}).", if removed == 1 { "y" } else { "ies" }), obj! { "scope" => scope, "removed" => removed }, &[])
}

/// Method-name prefixes that indicate state changes; refused by the passthrough.
const WRITE_VERBS: &[&str] = &["Set", "Grant", "Revoke", "Add", "Delete", "Remove", "Update", "Report", "Finalize", "Consume", "Exchange", "Modify", "Post", "Publish", "Ban", "Unban", "Init", "Refund", "Cancel", "Create", "Trigger", "Adjust", "Request", "Upload"];

fn api_call(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let iface = arg_str(args, "interface").unwrap_or("");
    let method = arg_str(args, "method").unwrap_or("");
    let ident = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ident(iface) || !ident(method) {
        return tool_error("'interface' and 'method' must be Web API identifiers such as ISteamNews and GetNewsForApp.");
    }
    if WRITE_VERBS.iter().any(|v| method.starts_with(v)) {
        return tool_error(format!("'{method}' looks state-changing; the passthrough only performs read-only GET calls. Use a dedicated tool instead."));
    }
    let version = arg_u64(args, "version").unwrap_or(1).clamp(1, 99);
    let partner = arg_str(args, "host") == Some("partner");
    let ttl = match arg_str(args, "ttl_class").unwrap_or("short") {
        "realtime" => "realtime",
        "medium" => "medium",
        "long" => "long",
        "static" => "static",
        "volatile" => "volatile",
        _ => "short",
    };
    let (host, key) = if partner { (Host::Partner, KeyKind::Publisher) } else { (Host::Public, KeyKind::Optional) };
    let mut call = Call::get(host, &format!("/{iface}/{method}/v{version}/"), key, ttl);
    if let Some(Value::Object(params)) = args.get("params") {
        for (k, v) in params.iter() {
            if k.eq_ignore_ascii_case("key") || k.eq_ignore_ascii_case("format") {
                continue;
            }
            match v {
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        call = call.param(&format!("{k}[{i}]"), item.to_param_string());
                    }
                }
                Value::Object(_) => call = call.param(k, v.to_string()),
                _ => call = call.param(k, v.to_param_string()),
            }
        }
    }
    let l = try_steam!(env.steam().call(&call, force(args)));
    if l.negative {
        return tool_error(format!("{iface}/{method}/v{version} returned no data (unknown method/version or empty response)."));
    }
    let text = crate::util::truncate(&l.value.to_string(), 200);
    ok_result(format!("{iface}/{method}/v{version} on {}: {text}", host.as_str()), (*l.value).clone(), &[&l])
}

// ---------------------------------------------------------------------------
// export_report (MRTR: roots/list)
// ---------------------------------------------------------------------------

fn sanitize_filename(name: &str) -> Option<String> {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
    if n.is_empty() || n.starts_with('.') || n.len() > 120 || n != name { None } else { Some(n) }
}

fn root_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // file:///abs/path or file://localhost/abs/path
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let p = PathBuf::from(pct_decode(rest, false));
    p.is_absolute().then_some(p)
}

/// Finds the most table-like array in a value: the value itself, or the
/// largest array of objects among its fields (one level deep).
fn tabular(v: &Value) -> Option<&Vec<Value>> {
    if let Value::Array(a) = v {
        return Some(a);
    }
    let mut best: Option<&Vec<Value>> = None;
    for (_, field) in v.as_object()?.iter() {
        let candidate = match field {
            Value::Array(a) if a.iter().any(|x| x.is_object()) => Some(a),
            Value::Object(_) => tabular(field).filter(|a| a.iter().any(|x| x.is_object())),
            _ => None,
        };
        if let Some(a) = candidate {
            if best.is_none_or(|b| a.len() > b.len()) {
                best = Some(a);
            }
        }
    }
    best
}

fn flatten(prefix: &str, v: &Value, out: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(m) if prefix.matches('.').count() < 2 => {
            for (k, val) in m.iter() {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, val, out);
            }
        }
        Value::Null => {
            out.insert(prefix.to_string(), String::new());
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        other => {
            out.insert(prefix.to_string(), if other.is_object() || other.is_array() { other.to_string() } else { other.to_param_string() });
        }
    }
}

fn csv_escape(s: &str) -> String {
    // Neutralise spreadsheet formula injection, then quote if needed.
    let s = if s.starts_with(['=', '+', '-', '@']) && s.parse::<f64>().is_err() { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

pub fn to_csv(rows: &[Value]) -> String {
    let flat: Vec<BTreeMap<String, String>> = rows.iter().map(|r| {
        let mut m = BTreeMap::new();
        flatten("", r, &mut m);
        if !r.is_object() {
            m.insert("value".into(), r.to_param_string());
        }
        m
    }).collect();
    let mut cols: Vec<String> = Vec::new();
    for m in flat.iter().take(500) {
        for k in m.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let mut out = cols.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(",");
    out.push('\n');
    for m in &flat {
        let line: Vec<String> = cols.iter().map(|c| csv_escape(m.get(c).map(String::as_str).unwrap_or(""))).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

fn export_report(env: &ToolEnv, args: &Map) -> ToolOutcome {
    if env.ctx.transport == Transport::Http {
        return tool_error("export_report writes to the local filesystem and is only available over the stdio transport.");
    }
    let tool_name = arg_str(args, "tool").unwrap_or("");
    let tools = all_tools();
    let Some(inner) = tools.iter().find(|t| t.name == tool_name) else {
        return tool_error(format!("Unknown tool '{tool_name}'."));
    };
    if !inner.hints.read_only || inner.write || inner.name == "export_report" || inner.name == "steam_summarize_reviews" {
        return tool_error(format!("'{tool_name}' cannot be exported; only read-only data tools are supported."));
    }
    if !env.ctx.principal.has_scope(inner.scope) {
        return tool_error(format!("'{tool_name}' requires scope {}.", inner.scope));
    }

    // Resolve the output directory: explicit argument → client roots (MRTR) → configured default.
    let explicit = arg_str(args, "directory").map(|d| root_to_path(d).unwrap_or_else(|| PathBuf::from(d)));
    let from_roots = env.mrtr.response("roots").and_then(|r| r["roots"].as_array()).and_then(|roots| roots.iter().find_map(|r| r["uri"].as_str().and_then(root_to_path)));
    let dir = match (explicit, from_roots) {
        (Some(d), _) => d,
        (None, Some(d)) => d,
        (None, None) if env.mrtr.response("roots").is_none() && env.ctx.supports_roots() => {
            let mut requests = Map::new();
            requests.insert("roots", obj! { "method" => "roots/list", "params" => obj! {} });
            return ToolOutcome::InputRequired { requests, state: Some(obj! { "step" => "roots" }) };
        }
        (None, None) => match env.cfg().export_dir.clone() {
            Some(d) => d,
            None => return tool_error("No output directory: pass 'directory', use a client that offers roots, or set STEAM_MCP_EXPORT_DIR."),
        },
    };
    if !dir.is_absolute() || !dir.is_dir() {
        return tool_error(format!("'{}' is not an existing absolute directory.", dir.display()));
    }
    if dir.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return tool_error("Directory paths may not contain '..'.");
    }

    let inner_args = args.get("arguments").and_then(|a| a.as_object()).cloned().unwrap_or_default();
    let mut inner_value = Value::Object(inner_args);
    crate::mcp::schema::coerce(&inner.input_schema, &mut inner_value);
    if let Err(errs) = crate::mcp::schema::validate(&inner.input_schema, &inner_value) {
        return tool_error(format!("Invalid arguments for {tool_name}: {}", errs.join("; ")));
    }
    let inner_env = ToolEnv { core: env.core, ctx: env.ctx, mrtr: Mrtr::default(), task: None };
    let result = match (inner.handler)(&inner_env, inner_value.as_object().unwrap_or(&Map::new())) {
        ToolOutcome::Done(r) => r,
        ToolOutcome::InputRequired { .. } => return tool_error(format!("'{tool_name}' needs interactive input and cannot be exported.")),
        ToolOutcome::Error(e) => return ToolOutcome::Error(e),
    };
    if result["isError"].as_bool() == Some(true) {
        return ToolOutcome::Done(result);
    }
    let data = result["structuredContent"].clone();
    let format = match arg_str(args, "format").unwrap_or("auto") {
        "csv" => "csv",
        "json" => "json",
        _ if tabular(&data).is_some() => "csv",
        _ => "json",
    };
    let body = match format {
        "csv" => match tabular(&data) {
            Some(rows) => to_csv(rows),
            None => return tool_error("The result has no tabular data; use format=json."),
        },
        _ => data.to_pretty_string(),
    };
    let default_name = format!("steam-{}-{}.{format}", tool_name.trim_start_matches("steam_"), crate::util::time::iso8601_secs(crate::util::time::now_secs()).replace([':', '-'], ""));
    let name = match arg_str(args, "filename") {
        Some(n) => match sanitize_filename(n) {
            Some(n) => n,
            None => return tool_error("'filename' may only contain letters, digits, '.', '_' and '-' and must not start with '.'."),
        },
        None => default_name,
    };
    let path = dir.join(&name);
    if path.parent() != Some(Path::new(&dir)) {
        return tool_error("Invalid output path.");
    }
    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return tool_error(format!("{} already exists; choose another filename.", path.display())),
        Err(e) => return tool_error(format!("Could not create {}: {e}", path.display())),
    };
    if let Err(e) = file.write_all(body.as_bytes()).and_then(|_| file.sync_all()) {
        return tool_error(format!("Could not write {}: {e}", path.display()));
    }
    let uri = format!("file://{}", path.display());
    let mut out = obj! {
        "content" => crate::arr![
            obj! { "type" => "text", "text" => format!("Exported {tool_name} as {format} ({} bytes) to {}", body.len(), path.display()) },
            obj! { "type" => "resource_link", "uri" => uri.as_str(), "name" => name.as_str(), "mimeType" => if format == "csv" { "text/csv" } else { "application/json" }, "size" => body.len() },
        ],
        "structuredContent" => obj! { "path" => path.display().to_string(), "uri" => uri.as_str(), "format" => format, "bytes" => body.len() },
        "isError" => false,
    };
    if let Some(m) = result.get("_meta") {
        out.set("_meta", m.clone());
    }
    ToolOutcome::Done(out)
}

// ---------------------------------------------------------------------------
// steam_summarize_reviews (MRTR: sampling/createMessage)
// ---------------------------------------------------------------------------

const STOPWORDS: &[&str] = &["the", "and", "a", "to", "of", "is", "it", "i", "this", "that", "in", "for", "you", "but", "with", "game", "on", "are", "be", "not", "have", "was", "as", "so", "just", "my", "if", "its", "it's", "can", "all", "they", "at", "or", "like", "me", "an", "do", "there", "get", "one", "more", "very", "what", "when", "out", "has", "from", "about", "really", "would", "up", "no", "your", "even", "some", "time", "dont", "don't", "will", "only", "good", "play", "games", "much", "by", "than", "also", "other", "been", "which", "make", "them", "we", "how", "were", "had", "any", "after", "then", "still", "playing", "played", "because", "lot", "into", "could", "way", "their", "too", "im", "i'm", "it", "is", "10", "s", "t"];

fn top_terms(reviews: &[&Value], n: usize) -> Vec<Value> {
    let mut freq: BTreeMap<String, u64> = BTreeMap::new();
    for r in reviews {
        let text = r["review"].as_str().unwrap_or("").to_lowercase();
        let mut seen = std::collections::HashSet::new();
        for w in text.split(|c: char| !c.is_alphanumeric() && c != '\'') {
            let w = w.trim_matches('\'');
            if w.len() < 3 || STOPWORDS.contains(&w) || w.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if seen.insert(w.to_string()) {
                *freq.entry(w.to_string()).or_default() += 1;
            }
        }
    }
    let mut v: Vec<(String, u64)> = freq.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(n).map(|(w, c)| obj! { "term" => w, "reviews" => c }).collect()
}

fn summarize_reviews_tool(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let n = arg_u64(args, "num_reviews").unwrap_or(50).clamp(5, 200);
    let mut fetch_args = args.clone();
    fetch_args.insert("filter", arg_str(args, "filter").unwrap_or("all"));
    let pages = n.div_ceil(100);
    let (qs, reviews, _, lookups) = try_steam!(super::public::fetch_reviews(env, appid, &fetch_args, n.min(100), pages));
    let reviews: Vec<Value> = reviews.into_iter().take(n as usize).collect();
    if reviews.is_empty() {
        return tool_error(format!("App {appid} has no reviews matching these filters."));
    }
    let summary = super::public::summarize_reviews(&qs);
    let pos: Vec<&Value> = reviews.iter().filter(|r| r["voted_up"].as_bool() == Some(true)).collect();
    let neg: Vec<&Value> = reviews.iter().filter(|r| r["voted_up"].as_bool() != Some(true)).collect();
    let stats = obj! {
        "appid" => appid,
        "overall" => summary,
        "sampleSize" => reviews.len(),
        "samplePositive" => pos.len(),
        "sampleNegative" => neg.len(),
        "positiveTerms" => top_terms(&pos, 15),
        "negativeTerms" => top_terms(&neg, 15),
    };
    let refs: Vec<&Lookup> = lookups.iter().collect();

    // Retry carrying the client's sampling result.
    let sampled_step = env.mrtr.state.as_ref().is_some_and(|s| s["step"].as_str() == Some("sampling"));
    if let (true, Some(msg)) = (sampled_step, env.mrtr.response("summary")) {
        let content = &msg["content"];
        let text = match content {
            Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
            other => other["text"].as_str().unwrap_or("").to_string(),
        };
        if !text.trim().is_empty() {
            return ok_result(text.clone(), stats.with("summary", text).with("model", msg["model"].clone()).with("method", "sampling"), &refs);
        }
    }

    if env.ctx.supports_sampling() && env.mrtr.response("summary").is_none() {
        let focus = arg_str(args, "focus").unwrap_or("overall player sentiment, the most praised aspects, the most common complaints, and actionable recommendations for the developer");
        let mut corpus = String::new();
        for r in &reviews {
            let line = format!("[{}] {}\n", if r["voted_up"].as_bool() == Some(true) { "+" } else { "-" }, crate::util::truncate(&r["review"].as_str().unwrap_or("").replace('\n', " "), 600));
            if corpus.len() + line.len() > 24_000 {
                break;
            }
            corpus.push_str(&line);
        }
        let mut requests = Map::new();
        requests.insert("summary", obj! {
            "method" => "sampling/createMessage",
            "params" => obj! {
                "messages" => crate::arr![obj! { "role" => "user", "content" => obj! { "type" => "text", "text" => format!("Here are {} Steam user reviews for app {appid} ('+' = recommended, '-' = not recommended). Overall: {}.\n\nSummarize {focus}. Be concise and specific; quote short phrases where useful.\n\n{corpus}", reviews.len(), stats["overall"]) } }],
                "systemPrompt" => "You are an analyst summarizing video game player reviews for a game studio. Be factual and balanced.",
                "includeContext" => "none",
                "maxTokens" => 900,
                "temperature" => 0.3,
                "modelPreferences" => obj! { "intelligencePriority" => 0.7, "speedPriority" => 0.4, "costPriority" => 0.4 },
            },
        });
        return ToolOutcome::InputRequired { requests, state: Some(obj! { "step" => "sampling" }) };
    }

    // Deterministic fallback without an LLM.
    let pt: Vec<String> = stats["positiveTerms"].as_array().map(|a| a.iter().take(6).filter_map(|t| t["term"].as_str().map(String::from)).collect()).unwrap_or_default();
    let nt: Vec<String> = stats["negativeTerms"].as_array().map(|a| a.iter().take(6).filter_map(|t| t["term"].as_str().map(String::from)).collect()).unwrap_or_default();
    let text = format!(
        "App {appid}: {} of {} sampled reviews are positive. Overall rating {}. Recurring themes in positive reviews: {}. In negative reviews: {}.",
        pos.len(),
        reviews.len(),
        stats.at(&["overall", "reviewScoreDescription"]).as_str().unwrap_or("n/a"),
        if pt.is_empty() { "n/a".into() } else { pt.join(", ") },
        if nt.is_empty() { "n/a".into() } else { nt.join(", ") }
    );
    ok_result(text, stats.with("method", "term-frequency"), &refs)
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "cache_stats",
            title: "Cache Statistics",
            description: "Cache and upstream statistics: entries, memory, hit ratio, stale serves, coalesced requests, upstream calls, errors, latency, daily budget and TTL classes.".into(),
            input_schema: object_schema(vec![], &[]),
            output_schema: Some(obj! { "type" => "object", "properties" => obj! { "cache" => obj! { "type" => "object" }, "upstream" => obj! { "type" => "object" }, "namespaces" => obj! { "type" => "object" }, "ttlClasses" => obj! { "type" => "object" } }, "required" => crate::arr!["cache", "upstream", "namespaces", "ttlClasses"] }),
            hints: LOCAL,
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            task: false,
            write: false,
            handler: cache_stats,
        },
        ToolDef {
            name: "cache_invalidate",
            title: "Invalidate Cache",
            description: "Drops cached Steam data so the next request fetches fresh data: everything, one app, one player, the app catalog, or a key prefix.".into(),
            input_schema: object_schema(vec![
                ("scope", p_enum("What to invalidate.", &["app", "player", "catalog", "prefix", "all"], Some("app"))),
                ("appid", p_appid_plain("App ID for scope=app.")),
                ("steamid", p_steamid()),
                ("prefix", p_str("Cache key prefix for scope=prefix, e.g. 'store:/api/appdetails'.")),
            ], &["scope"]),
            output_schema: Some(obj! { "type" => "object", "properties" => obj! { "scope" => obj! { "type" => "string" }, "removed" => obj! { "type" => "integer" } }, "required" => crate::arr!["scope", "removed"] }),
            hints: Hints { read_only: false, destructive: false, idempotent: true, open_world: false },
            scope: scopes::ADMIN,
            key: KeyKind::None,
            task: false,
            write: false,
            handler: cache_invalidate,
        },
        ToolDef {
            name: "steam_api_call",
            title: "Steam Web API Call",
            description: "Calls any read-only Steam Web API GET method not covered by a dedicated tool (see steam_get_supported_api_list). Results are cached under the chosen TTL class. Keys are injected automatically; state-changing methods are refused.".into(),
            input_schema: object_schema(vec![
                ("interface", obj! { "type" => "string", "minLength" => 1, "maxLength" => 64, "description" => "Interface name, e.g. ISteamUserStats." }),
                ("method", obj! { "type" => "string", "minLength" => 1, "maxLength" => 64, "description" => "Method name, e.g. GetNumberOfCurrentPlayers." }),
                ("version", p_int("Method version number.", 1, 99, Some(1))),
                ("params", obj! { "type" => "object", "description" => "Query parameters. Arrays expand to name[0], name[1], …", "additionalProperties" => obj! { "type" => ["string", "number", "integer", "boolean", "array", "object"] } }),
                ("host", p_enum("public = api.steampowered.com, partner = partner.steam-api.com (publisher key).", &["public", "partner"], Some("public"))),
                ("ttl_class", p_enum("Cache TTL class for the result.", &["volatile", "realtime", "short", "medium", "long", "static"], Some("short"))),
                p_force(),
            ], &["interface", "method"]),
            output_schema: None,
            hints: READ,
            scope: scopes::PARTNER,
            key: KeyKind::None,
            task: false,
            write: false,
            handler: api_call,
        },
        ToolDef {
            name: "export_report",
            title: "Export Report",
            description: "Runs a read-only data tool and writes its result to a new CSV or JSON file in a client-provided root directory (stdio transport only). Never overwrites files.".into(),
            input_schema: object_schema(vec![
                ("tool", obj! { "type" => "string", "minLength" => 1, "description" => "Name of the read-only tool to run, e.g. steam_partner_sales_summary." }),
                ("arguments", obj! { "type" => "object", "description" => "Arguments for that tool." }),
                ("format", p_enum("Output format (auto picks CSV for tabular results).", &["auto", "csv", "json"], Some("auto"))),
                ("filename", p_str("File name (letters, digits, '.', '_', '-'). Defaults to a timestamped name.")),
                ("directory", p_str("Absolute directory or file:// URI. Defaults to the client's first root.")),
            ], &["tool"]),
            output_schema: Some(obj! { "type" => "object", "properties" => obj! { "path" => obj! { "type" => "string" }, "uri" => obj! { "type" => "string" }, "format" => obj! { "type" => "string" }, "bytes" => obj! { "type" => "integer" } }, "required" => crate::arr!["path", "uri", "format", "bytes"] }),
            hints: Hints { read_only: false, destructive: false, idempotent: false, open_world: false },
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            task: false,
            write: false,
            handler: export_report,
        },
        ToolDef {
            name: "steam_summarize_reviews",
            title: "Summarize Reviews",
            description: "Summarizes a sample of an app's reviews. If the client supports sampling, asks the client's LLM for a written analysis; otherwise returns term-frequency themes for positive and negative reviews.".into(),
            input_schema: object_schema(vec![
                ("appid", p_appid()),
                ("num_reviews", p_int("Reviews to sample.", 5, 200, Some(50))),
                ("filter", p_enum("Which reviews to sample.", &["all", "recent", "updated"], Some("all"))),
                ("language", p_str("Steam language name or 'all'.")),
                ("review_type", p_enum("Restrict to positive or negative.", &["all", "positive", "negative"], Some("all"))),
                ("focus", p_str("What the summary should focus on.")),
                p_force(),
            ], &["appid"]),
            output_schema: None,
            hints: READ,
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            task: false,
            write: false,
            handler: summarize_reviews_tool,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_and_tabular() {
        let v = crate::json::parse(r#"{"meta":1,"rows":[{"a":1,"b":{"c":"x,y"}},{"a":"=SUM(A1)","d":null}]}"#).unwrap();
        let rows = tabular(&v).unwrap();
        let csv = to_csv(rows);
        assert_eq!(csv, "a,b.c,d\n1,\"x,y\",\n'=SUM(A1),,\n");
        assert!(sanitize_filename("../x").is_none());
        assert!(sanitize_filename(".hidden").is_none());
        assert_eq!(sanitize_filename("report-1.csv").as_deref(), Some("report-1.csv"));
        assert_eq!(root_to_path("file:///tmp/a%20b"), Some(PathBuf::from("/tmp/a b")));
        assert_eq!(root_to_path("https://x"), None);
    }
}
