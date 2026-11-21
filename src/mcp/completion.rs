//! `completion/complete` for prompt arguments and resource template
//! variables: app IDs (partner apps first, then the cached catalog),
//! languages, countries and sales dates.

use std::sync::Arc;

use super::context::RequestContext;
use super::server::Core;
use super::tools::{Mrtr, ToolEnv};
use crate::steam::KeyKind;

pub const LANGUAGES: &[&str] = &[
    "arabic", "brazilian", "bulgarian", "czech", "danish", "dutch", "english", "finnish", "french", "german", "greek", "hungarian", "indonesian", "italian", "japanese", "koreana", "latam", "norwegian", "polish", "portuguese", "romanian", "russian", "schinese", "spanish", "swedish", "tchinese", "thai", "turkish", "ukrainian", "vietnamese",
];

pub const COUNTRIES: &[&str] = &[
    "ae", "ar", "at", "au", "be", "bg", "br", "ca", "ch", "cl", "cn", "co", "cr", "cz", "de", "dk", "ee", "eg", "es", "fi", "fr", "gb", "gr", "hk", "hr", "hu", "id", "ie", "il", "in", "is", "it", "jp", "kr", "kw", "kz", "lt", "lu", "lv", "mx", "my", "ng", "nl", "no", "nz", "pe", "ph", "pk", "pl", "pt", "qa", "ro", "rs", "ru", "sa", "se", "sg", "si", "sk", "th", "tr", "tw", "ua", "us", "uy", "vn", "za",
];

pub const MAX_VALUES: usize = 100;

/// Completion result: (values, total, hasMore).
pub type Completion = (Vec<String>, usize, bool);

fn finish(mut all: Vec<String>) -> Completion {
    all.dedup();
    let total = all.len();
    let has_more = total > MAX_VALUES;
    all.truncate(MAX_VALUES);
    (all, total, has_more)
}

fn prefix_list(list: &[&str], value: &str) -> Completion {
    let v = value.trim().to_ascii_lowercase();
    finish(list.iter().filter(|x| x.starts_with(&v)).map(|x| x.to_string()).collect())
}

fn complete_appid(core: &Arc<Core>, ctx: &RequestContext, value: &str) -> Completion {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    // Partner apps (cached) rank first.
    if core.steam.has_key(KeyKind::Publisher) && ctx.principal.has_scope(super::context::scopes::PARTNER) {
        let env = ToolEnv { core, ctx, mrtr: Mrtr::default(), task: None };
        if let Ok(apps) = super::tools::partner::partner_apps(&env) {
            for a in apps {
                let id = a["appid"].to_param_string();
                let name = a["app_name"].as_str().unwrap_or("").to_ascii_lowercase();
                if id.starts_with(v) || (!lower.is_empty() && name.contains(&lower)) {
                    out.push(id);
                }
            }
        }
    }
    match core.steam.catalog_if_cached() {
        Some(index) => {
            if v.is_empty() {
                // Nothing typed yet: offer nothing beyond partner apps.
            } else if v.bytes().all(|b| b.is_ascii_digit()) {
                // App IDs are sorted numerically, not lexicographically: scan for prefixes.
                out.extend(index.iter().filter(|e| e.appid.to_string().starts_with(v)).take(500).map(|e| e.appid.to_string()));
            } else {
                out.extend(crate::steam::catalog::search(&index, v, 200).into_iter().map(|(e, _)| e.appid.to_string()));
            }
        }
        None => {
            // Warm the catalog in the background so later completions work.
            let steam = core.steam.clone();
            let _ = std::thread::Builder::new().name("catalog-warm".into()).spawn(move || {
                let _ = steam.catalog(false);
            });
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|x| seen.insert(x.clone()));
    finish(out)
}

fn complete_date(core: &Arc<Core>, ctx: &RequestContext, value: &str) -> Completion {
    if !core.steam.has_key(KeyKind::Financial) || !ctx.principal.has_scope(super::context::scopes::FINANCIAL) {
        return (Vec::new(), 0, false);
    }
    let env = ToolEnv { core, ctx, mrtr: Mrtr::default(), task: None };
    match super::tools::partner::changed_dates(&env, "0", false) {
        Ok((mut dates, _, _)) => {
            dates.sort();
            dates.reverse();
            finish(dates.into_iter().filter(|d| d.starts_with(value.trim())).collect())
        }
        Err(_) => (Vec::new(), 0, false),
    }
}

/// Completes `argument` given its name and current value.
pub fn complete(core: &Arc<Core>, ctx: &RequestContext, arg_name: &str, value: &str) -> Completion {
    match arg_name {
        "appid" => complete_appid(core, ctx, value),
        "competitors" | "appids" => {
            // Complete the last element of a comma-separated list.
            let (head, last) = match value.rsplit_once(',') {
                Some((h, l)) => (format!("{h},"), l.trim()),
                None => (String::new(), value),
            };
            let (vals, total, more) = complete_appid(core, ctx, last);
            (vals.into_iter().map(|v| format!("{head}{v}")).collect(), total, more)
        }
        "language" => prefix_list(LANGUAGES, value),
        "cc" => prefix_list(COUNTRIES, value),
        "date" => complete_date(core, ctx, value),
        "focus" => prefix_list(&["performance", "monetization", "difficulty", "content", "multiplayer", "bugs", "story", "controls", "servers", "pricing"], value),
        _ => (Vec::new(), 0, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_lists() {
        assert_eq!(prefix_list(LANGUAGES, "sch").0, vec!["schinese"]);
        assert!(prefix_list(COUNTRIES, "").0.len() <= MAX_VALUES);
        let (v, total, more) = finish((0..150).map(|i| i.to_string()).collect());
        assert_eq!((v.len(), total, more), (100, 150, true));
    }
}
