//! Prompt templates. Each prompt embeds live (cached) Steam data as embedded
//! resources and gives the model a focused analysis brief.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::context::{RequestContext, scopes};
use super::protocol::McpError;
use super::server::Core;
use crate::json::{Map, Value};
use crate::obj;
use crate::steam::KeyKind;

pub struct PromptArg {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub required: bool,
}

pub struct PromptDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub args: Vec<PromptArg>,
    pub scope: &'static str,
    pub key: KeyKind,
    /// Resource URIs (templated with argument values) to embed.
    pub embeds: &'static [&'static str],
    /// The analysis brief; `{name}` placeholders are replaced with arguments.
    pub brief: &'static str,
}

const fn arg(name: &'static str, title: &'static str, description: &'static str, required: bool) -> PromptArg {
    PromptArg { name, title, description, required }
}

pub fn prompts() -> Vec<PromptDef> {
    vec![
        PromptDef {
            name: "game_health_report",
            title: "Game Health Report",
            description: "Assess an app's current health: player activity, review sentiment, pricing, recent updates and achievement engagement.",
            args: vec![arg("appid", "App ID", "Steam app ID to analyze", true)],
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            embeds: &["steam://app/{appid}/overview"],
            brief: "Write a concise health report for Steam app {appid} using the embedded overview. Cover: (1) player activity and what the current player count suggests, (2) review sentiment and its trend, (3) pricing and positioning, (4) cadence and content of recent news/updates, (5) achievement engagement. Finish with the three highest-impact recommendations. Use steam_get_app_reviews, steam_get_app_news or steam_get_global_achievement_percentages if you need more detail.",
        },
        PromptDef {
            name: "review_sentiment_analysis",
            title: "Review Sentiment Analysis",
            description: "Analyze recent player reviews for themes, praise, complaints and emerging issues.",
            args: vec![arg("appid", "App ID", "Steam app ID", true), arg("focus", "Focus", "Optional aspect to focus on (e.g. performance, monetization)", false)],
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            embeds: &["steam://app/{appid}/reviews"],
            brief: "Analyze the embedded recent reviews for app {appid}. Identify the main positive and negative themes with representative short quotes, note anything trending or newly emerging, weigh reviews by playtime where relevant, and focus especially on: {focus}. Conclude with prioritized actions for the developer.",
        },
        PromptDef {
            name: "player_profile_report",
            title: "Player Profile Report",
            description: "Summarize a Steam player's profile, activity and account standing.",
            args: vec![arg("steamid", "SteamID", "64-bit SteamID of the player", true)],
            scope: scopes::PUBLIC,
            key: KeyKind::Public,
            embeds: &["steam://player/{steamid}/profile"],
            brief: "Summarize the embedded profile for SteamID {steamid}: account age, visibility, level, recent activity and any bans. Note what is hidden by privacy settings. Use steam_get_owned_games for library analysis if the profile is public.",
        },
        PromptDef {
            name: "achievement_design_review",
            title: "Achievement Design Review",
            description: "Review an app's achievement set: difficulty curve, completion funnel and design gaps.",
            args: vec![arg("appid", "App ID", "Steam app ID", true)],
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            embeds: &["steam://app/{appid}/achievements"],
            brief: "Review the achievement design of app {appid} from the embedded global unlock rates. Describe the difficulty distribution and the implied player funnel (where players drop off), flag achievements that are almost never or almost always unlocked, and suggest design improvements. Use steam_get_game_schema for names and descriptions if needed.",
        },
        PromptDef {
            name: "patch_notes_digest",
            title: "Patch Notes Digest",
            description: "Digest an app's recent news and patch notes into a changelog summary.",
            args: vec![arg("appid", "App ID", "Steam app ID", true)],
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            embeds: &["steam://app/{appid}/news"],
            brief: "Turn the embedded news items for app {appid} into a dated digest: group by update, list notable changes, and highlight balance changes, fixes and new content. Note the overall update cadence.",
        },
        PromptDef {
            name: "competitor_comparison",
            title: "Competitor Comparison",
            description: "Compare an app with competitors on price, players, reviews and content.",
            args: vec![arg("appid", "App ID", "Your app ID", true), arg("competitors", "Competitors", "Comma-separated competitor app IDs", true)],
            scope: scopes::PUBLIC,
            key: KeyKind::None,
            embeds: &["steam://app/{appid}/overview"],
            brief: "Compare app {appid} (embedded overview) against competitors {competitors}. Call steam_compare_apps with all of these app IDs, then explain relative strengths and weaknesses in price, concurrent players, review sentiment and content, and recommend positioning changes.",
        },
        PromptDef {
            name: "release_readiness_check",
            title: "Release Readiness Check",
            description: "Partner: review branches and recent builds before a release.",
            args: vec![arg("appid", "App ID", "Your app ID", true)],
            scope: scopes::PARTNER,
            key: KeyKind::Publisher,
            embeds: &["steam://partner/app/{appid}/betas", "steam://partner/app/{appid}/builds"],
            brief: "Using the embedded branches and builds for app {appid}, report which build is live on each branch, how far each branch is behind the newest build, any password-protected branches, and anything that looks risky before a release (e.g. public running an old build, undescribed builds). Do not change anything.",
        },
        PromptDef {
            name: "partner_sales_report",
            title: "Daily Sales Report",
            description: "Financial: summarize a day of Steam sales.",
            args: vec![arg("date", "Date", "Date from steam_partner_get_changed_sales_dates", true)],
            scope: scopes::FINANCIAL,
            key: KeyKind::Financial,
            embeds: &["steam://partner/sales/{date}"],
            brief: "Write a sales report for {date} from the embedded summary: totals (gross/net revenue and units), top apps, notable returns, and anything unusual. Use steam_partner_sales_summary with group_by [\"country_code\"] or [\"platform\"] for breakdowns.",
        },
    ]
}

/// Visible prompts for a caller.
pub fn visible<'a>(core: &Core, ctx: &RequestContext, defs: &'a [PromptDef]) -> Vec<&'a PromptDef> {
    defs.iter().filter(|p| ctx.principal.has_scope(p.scope) && core.steam.has_key(p.key)).collect()
}

pub fn to_json(p: &PromptDef) -> Value {
    let args: Vec<Value> = p.args.iter().map(|a| obj! { "name" => a.name, "title" => a.title, "description" => a.description, "required" => a.required }).collect();
    obj! { "name" => p.name, "title" => p.title, "description" => p.description, "arguments" => args, "icons" => crate::arr![super::protocol::server_icon()] }
}

fn fill(template: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    // Unfilled optional placeholders read naturally.
    let mut cleaned = String::new();
    let mut rest = out.as_str();
    while let Some(start) = rest.find('{') {
        match rest[start..].find('}') {
            Some(end) if rest[start + 1..start + end].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                cleaned.push_str(&rest[..start]);
                cleaned.push_str("the most important issues");
                rest = &rest[start + end + 1..];
            }
            _ => {
                cleaned.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    cleaned.push_str(rest);
    cleaned
}

/// Outcome of `prompts/get`.
pub enum PromptOutcome {
    Done(Value),
    /// Missing required arguments to collect through elicitation.
    NeedArgs(Vec<&'static str>),
}

/// Builds the `GetPromptResult`.
pub fn get(core: &Arc<Core>, ctx: &RequestContext, p: &PromptDef, args: &BTreeMap<String, String>) -> Result<PromptOutcome, McpError> {
    let missing: Vec<&'static str> = p.args.iter().filter(|a| a.required && args.get(a.name).is_none_or(|v| v.trim().is_empty())).map(|a| a.name).collect();
    if !missing.is_empty() {
        return Ok(PromptOutcome::NeedArgs(missing));
    }
    for (k, v) in args {
        if !p.args.iter().any(|a| a.name == k) {
            return Err(McpError::invalid_params(format!("Unknown argument '{k}' for prompt {}", p.name)));
        }
        if v.len() > 2000 {
            return Err(McpError::invalid_params(format!("Argument '{k}' is too long")));
        }
    }
    if let Some(a) = args.get("appid") {
        if a.trim().parse::<u64>().map_or(true, |v| v == 0) {
            return Err(McpError::invalid_params("appid must be a positive integer"));
        }
    }
    if let Some(s) = args.get("steamid") {
        if crate::steam::parse_steamid(s).is_none() {
            return Err(McpError::invalid_params("steamid must be a 64-bit SteamID"));
        }
    }
    let mut messages = Vec::new();
    for embed in p.embeds {
        let uri = fill(embed, args);
        match super::resources::read(core, &uri, ctx) {
            Ok(r) => {
                for c in r.contents {
                    messages.push(obj! { "role" => "user", "content" => obj! { "type" => "resource", "resource" => c, "annotations" => obj! { "audience" => crate::arr!["assistant"], "priority" => 0.9 } } });
                }
            }
            Err(e) => {
                messages.push(obj! { "role" => "user", "content" => obj! { "type" => "text", "text" => format!("(Data for {uri} could not be loaded: {}. Use the Steam tools to retrieve it.)", e.message) } });
            }
        }
    }
    messages.push(obj! { "role" => "user", "content" => obj! { "type" => "text", "text" => fill(p.brief, args) } });
    Ok(PromptOutcome::Done(obj! { "description" => format!("{} — {}", p.title, p.description), "messages" => messages }))
}

/// Elicitation request asking for missing prompt arguments.
pub fn elicit_missing(p: &PromptDef, missing: &[&str]) -> Map {
    let mut props = Map::new();
    for name in missing {
        if let Some(a) = p.args.iter().find(|a| a.name == *name) {
            props.insert(a.name, obj! { "type" => "string", "title" => a.title, "description" => a.description, "minLength" => 1 });
        }
    }
    let mut requests = Map::new();
    requests.insert("prompt_arguments", obj! {
        "method" => "elicitation/create",
        "params" => obj! {
            "mode" => "form",
            "message" => format!("The '{}' prompt needs: {}", p.title, missing.join(", ")),
            "requestedSchema" => obj! { "type" => "object", "properties" => Value::Object(props), "required" => missing.to_vec() },
        },
    });
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filling() {
        let args: BTreeMap<String, String> = [("appid".to_string(), "570".to_string())].into_iter().collect();
        assert_eq!(fill("steam://app/{appid}/overview", &args), "steam://app/570/overview");
        assert_eq!(fill("focus on {focus}.", &args), "focus on the most important issues.");
        assert_eq!(fill("json {\"a\": 1}", &args), "json {\"a\": 1}");
        for p in prompts() {
            for e in p.embeds {
                assert!(super::super::resources::resolve(&e.replace("{appid}", "1").replace("{steamid}", "76561197960287930").replace("{date}", "2026-01-01")).is_some(), "{e}");
            }
        }
    }
}
