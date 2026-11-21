//! Resources and resource templates.
//!
//! Every resource is backed by a tool handler, so reads share the same cache
//! entries, TTL policies and error handling as tool calls. `ttlMs` on reads
//! reflects the remaining freshness of the underlying cache entries.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::context::{RequestContext, scopes};
use super::protocol::McpError;
use super::server::Core;
use super::tools::{META_CACHE, Mrtr, ToolEnv, ToolOutcome, all_tools};
use crate::json::{Map, Value};
use crate::obj;
use crate::steam::KeyKind;
use crate::util::uritemplate::UriTemplate;

/// How a resource maps onto a tool call.
pub type ArgsFn = fn(&BTreeMap<String, String>) -> Result<Map, McpError>;

pub struct ResourceSpec {
    pub uri: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub tool: &'static str,
    pub args: ArgsFn,
    /// `public` when the content does not depend on the caller.
    pub cache_scope: &'static str,
    pub priority: f64,
}

fn no_args(_: &BTreeMap<String, String>) -> Result<Map, McpError> {
    Ok(Map::new())
}

fn appid_args(v: &BTreeMap<String, String>) -> Result<Map, McpError> {
    let appid: u64 = v
        .get("appid")
        .and_then(|a| a.parse().ok())
        .filter(|a| *a > 0)
        .ok_or_else(|| {
            McpError::invalid_params("Resource not found: appid must be a positive integer")
        })?;
    let mut m = Map::new();
    m.insert("appid", appid);
    Ok(m)
}

fn steamid_args(v: &BTreeMap<String, String>) -> Result<Map, McpError> {
    let id = v
        .get("steamid")
        .and_then(|s| crate::steam::parse_steamid(s))
        .ok_or_else(|| {
            McpError::invalid_params("Resource not found: steamid must be a 64-bit SteamID")
        })?;
    let mut m = Map::new();
    m.insert("steamid", id.to_string());
    Ok(m)
}

fn reviews_args(v: &BTreeMap<String, String>) -> Result<Map, McpError> {
    let mut m = appid_args(v)?;
    m.insert("num_per_page", 50);
    m.insert("filter", "recent");
    m.insert("max_review_chars", 500);
    Ok(m)
}

fn news_args(v: &BTreeMap<String, String>) -> Result<Map, McpError> {
    let mut m = appid_args(v)?;
    m.insert("count", 20);
    Ok(m)
}

fn date_args(v: &BTreeMap<String, String>) -> Result<Map, McpError> {
    let d = v
        .get("date")
        .filter(|d| d.len() >= 8 && d.len() <= 32)
        .ok_or_else(|| McpError::invalid_params("Resource not found: invalid date"))?;
    let mut m = Map::new();
    m.insert("date", d.as_str());
    Ok(m)
}

fn builds_args(v: &BTreeMap<String, String>) -> Result<Map, McpError> {
    let mut m = appid_args(v)?;
    m.insert("count", 25);
    Ok(m)
}

fn catalog_args(_: &BTreeMap<String, String>) -> Result<Map, McpError> {
    // A cheap search over the cached catalog reports its size and freshness.
    let mut m = Map::new();
    m.insert("query", "steam");
    m.insert("limit", 1);
    Ok(m)
}

pub fn static_resources() -> Vec<ResourceSpec> {
    vec![
        ResourceSpec {
            uri: "steam://server/cache-stats",
            name: "cache-stats",
            title: "Cache Statistics",
            description: "Live cache and upstream statistics for this server.",
            tool: "cache_stats",
            args: no_args,
            cache_scope: "public",
            priority: 0.2,
        },
        ResourceSpec {
            uri: "steam://store/featured",
            name: "store-featured",
            title: "Steam Store Featured",
            description: "Featured store categories: specials, top sellers, new releases, coming soon.",
            tool: "steam_get_store_featured",
            args: no_args,
            cache_scope: "public",
            priority: 0.5,
        },
        ResourceSpec {
            uri: "steam://charts/most-played",
            name: "most-played",
            title: "Most Played Games",
            description: "Steam's current most-played games chart.",
            tool: "steam_get_most_played_games",
            args: no_args,
            cache_scope: "public",
            priority: 0.6,
        },
        ResourceSpec {
            uri: "steam://catalog/summary",
            name: "app-catalog",
            title: "App Catalog",
            description: "Size and freshness of the cached Steam app catalog used for search and completion.",
            tool: "steam_search_apps",
            args: catalog_args,
            cache_scope: "public",
            priority: 0.2,
        },
        ResourceSpec {
            uri: "steam://webapi/supported",
            name: "supported-apis",
            title: "Supported Web API Methods",
            description: "Web API interfaces and methods available to the configured key.",
            tool: "steam_get_supported_api_list",
            args: no_args,
            cache_scope: "private",
            priority: 0.3,
        },
        ResourceSpec {
            uri: "steam://partner/apps",
            name: "partner-apps",
            title: "Partner Apps",
            description: "Apps accessible with the configured Steamworks publisher key.",
            tool: "steam_partner_list_apps",
            args: no_args,
            cache_scope: "private",
            priority: 0.9,
        },
    ]
}

pub struct TemplateSpec {
    pub template: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub tool: &'static str,
    pub args: ArgsFn,
    pub cache_scope: &'static str,
}

pub fn templates() -> Vec<TemplateSpec> {
    vec![
        TemplateSpec {
            template: "steam://app/{appid}/details",
            name: "app-details",
            title: "App Store Details",
            description: "Store details for an app.",
            tool: "steam_get_app_details",
            args: appid_args,
            cache_scope: "public",
        },
        TemplateSpec {
            template: "steam://app/{appid}/overview",
            name: "app-overview",
            title: "App Overview",
            description: "Store details, players, reviews, news and achievements for an app.",
            tool: "steam_get_app_overview",
            args: appid_args,
            cache_scope: "public",
        },
        TemplateSpec {
            template: "steam://app/{appid}/players",
            name: "app-players",
            title: "Current Players",
            description: "Current concurrent players for an app.",
            tool: "steam_get_current_players",
            args: appid_args,
            cache_scope: "public",
        },
        TemplateSpec {
            template: "steam://app/{appid}/news",
            name: "app-news",
            title: "App News",
            description: "Recent news and patch notes for an app.",
            tool: "steam_get_app_news",
            args: news_args,
            cache_scope: "public",
        },
        TemplateSpec {
            template: "steam://app/{appid}/reviews",
            name: "app-reviews",
            title: "Recent Reviews",
            description: "Review summary and the 50 most recent reviews.",
            tool: "steam_get_app_reviews",
            args: reviews_args,
            cache_scope: "public",
        },
        TemplateSpec {
            template: "steam://app/{appid}/achievements",
            name: "app-achievements",
            title: "Global Achievement Rates",
            description: "Global achievement unlock percentages.",
            tool: "steam_get_global_achievement_percentages",
            args: appid_args,
            cache_scope: "public",
        },
        TemplateSpec {
            template: "steam://app/{appid}/schema",
            name: "app-schema",
            title: "Stats Schema",
            description: "Achievement and stat definitions.",
            tool: "steam_get_game_schema",
            args: appid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://player/{steamid}/profile",
            name: "player-profile",
            title: "Player Profile",
            description: "Profile overview: persona, level, bans, recent games.",
            tool: "steam_get_player_profile",
            args: steamid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://player/{steamid}/games",
            name: "player-games",
            title: "Owned Games",
            description: "A player's library with playtime.",
            tool: "steam_get_owned_games",
            args: steamid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://player/{steamid}/recent",
            name: "player-recent",
            title: "Recently Played",
            description: "Games played in the last two weeks.",
            tool: "steam_get_recently_played_games",
            args: steamid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://partner/app/{appid}/betas",
            name: "partner-betas",
            title: "App Branches",
            description: "Beta branches and live builds.",
            tool: "steam_partner_get_app_betas",
            args: appid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://partner/app/{appid}/builds",
            name: "partner-builds",
            title: "App Builds",
            description: "Recent builds.",
            tool: "steam_partner_get_app_builds",
            args: builds_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://partner/app/{appid}/depots",
            name: "partner-depots",
            title: "Depot Versions",
            description: "Live depot versions.",
            tool: "steam_partner_get_app_depot_versions",
            args: appid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://partner/app/{appid}/leaderboards",
            name: "partner-leaderboards",
            title: "Leaderboards",
            description: "Leaderboards for an app.",
            tool: "steam_partner_get_leaderboards",
            args: appid_args,
            cache_scope: "private",
        },
        TemplateSpec {
            template: "steam://partner/sales/{date}",
            name: "partner-sales",
            title: "Sales Summary",
            description: "Aggregated detailed sales for a date (financial key).",
            tool: "steam_partner_sales_summary",
            args: date_args,
            cache_scope: "private",
        },
    ]
}

/// The tool's scope and key requirement, used to filter visibility.
fn tool_requirements(tool: &str) -> Option<(&'static str, KeyKind)> {
    all_tools()
        .into_iter()
        .find(|t| t.name == tool)
        .map(|t| (t.scope, t.key))
}

/// Whether a resource backed by `tool` is available to this caller.
pub fn available(core: &Core, tool: &str, ctx: &RequestContext) -> bool {
    match tool_requirements(tool) {
        Some((scope, key)) => {
            ctx.principal.has_scope(scope)
                && core.steam.has_key(key)
                && !core.config.get().tool_disabled(tool)
        }
        None => false,
    }
}

pub enum Resolved {
    Static(ResourceSpec),
    Template(TemplateSpec, BTreeMap<String, String>),
    PartnerApp(u64),
}

/// Resolves a URI to a resource definition.
pub fn resolve(uri: &str) -> Option<Resolved> {
    if let Some(s) = static_resources().into_iter().find(|r| r.uri == uri) {
        return Some(Resolved::Static(s));
    }
    for t in templates() {
        if let Some(vars) = UriTemplate::parse(t.template).and_then(|tpl| tpl.matches(uri)) {
            return Some(Resolved::Template(t, vars));
        }
    }
    None
}

/// The resource result of reading a URI.
pub struct ReadResult {
    pub contents: Vec<Value>,
    pub ttl_ms: u64,
    pub cache_scope: &'static str,
    pub hash: String,
}

/// Remaining freshness of the cache entries behind a tool result.
fn remaining_ttl(result: &Value) -> u64 {
    let entries: Vec<Value> = match &result["_meta"][META_CACHE] {
        Value::Array(a) => a.clone(),
        v @ Value::Object(_) => vec![v.clone()],
        _ => Vec::new(),
    };
    entries
        .iter()
        .filter_map(|e| Some(e["ttlMs"].as_u64()?.saturating_sub(e["ageMs"].as_u64()?)))
        .min()
        .unwrap_or(5_000)
}

fn last_modified(result: &Value) -> Option<String> {
    match &result["_meta"][META_CACHE] {
        Value::Array(a) => a
            .iter()
            .filter_map(|e| e["fetchedAt"].as_str())
            .min()
            .map(String::from),
        v => v["fetchedAt"].as_str().map(String::from),
    }
}

/// Reads a resource by running its backing tool.
pub fn read(core: &Arc<Core>, uri: &str, ctx: &RequestContext) -> Result<ReadResult, McpError> {
    let not_found = || {
        McpError::invalid_params(format!("Resource not found: {uri}"))
            .with_data(obj! { "uri" => uri })
    };
    let (tool_name, args, cache_scope) = match resolve(uri).ok_or_else(not_found)? {
        Resolved::Static(s) => (s.tool, (s.args)(&BTreeMap::new())?, s.cache_scope),
        Resolved::Template(t, vars) => (
            t.tool,
            (t.args)(&vars).map_err(|_| not_found())?,
            t.cache_scope,
        ),
        Resolved::PartnerApp(_) => return Err(not_found()),
    };
    if !available(core, tool_name, ctx) {
        return Err(not_found());
    }
    let tools = all_tools();
    let tool = tools
        .iter()
        .find(|t| t.name == tool_name)
        .ok_or_else(not_found)?;
    let env = ToolEnv {
        core,
        ctx,
        mrtr: Mrtr::default(),
        task: None,
    };
    let result = match (tool.handler)(&env, &args) {
        ToolOutcome::Done(r) => r,
        ToolOutcome::InputRequired { .. } => {
            return Err(McpError::internal("resource unexpectedly required input"));
        }
        ToolOutcome::Error(e) => return Err(e),
    };
    if result["isError"].as_bool() == Some(true) {
        let msg = result
            .at(&["content", "0", "text"])
            .as_str()
            .unwrap_or("not found")
            .to_string();
        return Err(
            McpError::invalid_params(format!("Resource not found: {uri} ({msg})"))
                .with_data(obj! { "uri" => uri }),
        );
    }
    let data = result["structuredContent"].clone();
    let text = data.to_pretty_string();
    let hash = crate::util::crypto::sha256_hex(data.canonical().to_string().as_bytes());
    let mut content = obj! { "uri" => uri, "mimeType" => "application/json", "text" => text };
    if let Some(lm) = last_modified(&result) {
        content.set("_meta", obj! { "io.github.christianvozar/lastModified" => lm, META_CACHE => result["_meta"][META_CACHE].clone() });
    }
    Ok(ReadResult {
        contents: vec![content],
        ttl_ms: remaining_ttl(&result),
        cache_scope,
        hash,
    })
}

fn annotations(priority: f64) -> Value {
    obj! { "audience" => crate::arr!["user", "assistant"], "priority" => priority }
}

/// All listable resources (static + one details resource per partner app).
pub fn list(core: &Arc<Core>, ctx: &RequestContext) -> Vec<Value> {
    let mut out: Vec<Value> = static_resources()
        .into_iter()
        .filter(|r| available(core, r.tool, ctx))
        .map(|r| obj! { "uri" => r.uri, "name" => r.name, "title" => r.title, "description" => r.description, "mimeType" => "application/json", "annotations" => annotations(r.priority), "icons" => tool_icons(r.tool) })
        .collect();
    if available(core, "steam_partner_list_apps", ctx)
        && available(core, "steam_get_app_overview", ctx)
    {
        let env = ToolEnv {
            core,
            ctx,
            mrtr: Mrtr::default(),
            task: None,
        };
        if let Ok(apps) = super::tools::partner::partner_apps(&env) {
            let mut apps: Vec<(u64, String, String)> = apps
                .iter()
                .filter_map(|a| {
                    Some((
                        a["appid"].as_u64_lenient()?,
                        a["app_name"].as_str().unwrap_or("").to_string(),
                        a["app_type"].as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect();
            apps.sort_by_key(|a| a.0);
            for (appid, name, kind) in apps {
                out.push(obj! {
                    "uri" => format!("steam://app/{appid}/overview"),
                    "name" => format!("app-{appid}"),
                    "title" => if name.is_empty() { format!("App {appid}") } else { name.clone() },
                    "description" => format!("Overview of your {} {}", kind.to_ascii_lowercase(), if name.is_empty() { appid.to_string() } else { name }),
                    "mimeType" => "application/json",
                    "annotations" => annotations(0.8),
                    "icons" => crate::arr![super::protocol::scope_icon(scopes::PARTNER)],
                });
            }
        }
    }
    out
}

/// Icons for a resource, matching its backing tool's scope.
fn tool_icons(tool: &str) -> Value {
    let scope = tool_requirements(tool)
        .map(|(s, _)| s)
        .unwrap_or(scopes::PUBLIC);
    crate::arr![super::protocol::scope_icon(scope)]
}

pub fn list_templates(core: &Arc<Core>, ctx: &RequestContext) -> Vec<Value> {
    templates()
        .into_iter()
        .filter(|t| available(core, t.tool, ctx))
        .map(|t| obj! { "uriTemplate" => t.template, "name" => t.name, "title" => t.title, "description" => t.description, "mimeType" => "application/json", "annotations" => annotations(0.5), "icons" => tool_icons(t.tool) })
        .collect()
}

/// Whether listing or reading depends on the caller (affects `cacheScope`).
pub fn list_scope(core: &Core) -> &'static str {
    if core.config.get().auth.mode == crate::config::AuthMode::None {
        "public"
    } else {
        "private"
    }
}

pub fn requires_partner(uri: &str) -> bool {
    uri.starts_with("steam://partner/")
}

pub const SCOPE_FOR_LISTING: &str = scopes::PUBLIC;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_maps_to_a_tool() {
        let tools = all_tools();
        for r in static_resources() {
            assert!(tools.iter().any(|t| t.name == r.tool), "{}", r.uri);
            assert!(matches!(resolve(r.uri), Some(Resolved::Static(_))));
        }
        for t in templates() {
            assert!(tools.iter().any(|x| x.name == t.tool), "{}", t.template);
            assert!(UriTemplate::parse(t.template).is_some());
        }
        match resolve("steam://app/570/details") {
            Some(Resolved::Template(t, vars)) => {
                assert_eq!(t.tool, "steam_get_app_details");
                assert_eq!(vars["appid"], "570");
                assert_eq!(
                    appid_args(&vars)
                        .unwrap()
                        .get("appid")
                        .and_then(|v| v.as_u64()),
                    Some(570)
                );
            }
            _ => panic!("template did not resolve"),
        }
        assert!(resolve("steam://nope").is_none());
        assert!(
            appid_args(
                &[("appid".to_string(), "abc".to_string())]
                    .into_iter()
                    .collect()
            )
            .is_err()
        );
    }

    #[test]
    fn ttl_extraction() {
        let r = obj! { "_meta" => obj! { META_CACHE => crate::arr![obj! { "ttlMs" => 60_000, "ageMs" => 10_000, "fetchedAt" => "b" }, obj! { "ttlMs" => 30_000, "ageMs" => 5_000, "fetchedAt" => "a" }] } };
        assert_eq!(remaining_ttl(&r), 25_000);
        assert_eq!(last_modified(&r).as_deref(), Some("a"));
        assert_eq!(remaining_ttl(&obj! {}), 5_000);
    }
}
