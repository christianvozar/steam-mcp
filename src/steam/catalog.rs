//! The Steam app catalog: fetched once (paged), cached under the `static`
//! policy, persisted with the rest of the cache, and indexed in memory for
//! fast name search and argument completion.

use std::sync::{Arc, Mutex};

use super::{Call, ErrorKind, Host, KeyKind, SteamClient, SteamError};
use crate::cache::{FetchError, FetchFn, Fetched, Lookup};
use crate::json::Value;

/// One searchable app.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub appid: u64,
    pub name: String,
    norm: String,
}

/// In-memory search index derived from the cached catalog, rebuilt when the
/// cached value's hash changes.
#[derive(Default)]
pub struct CatalogIndex {
    built: Mutex<Option<(Arc<str>, Arc<Vec<CatalogEntry>>)>>,
}

/// Lowercases and replaces punctuation with spaces, collapsing runs.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        let c = if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' };
        if c == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim_end().to_string()
}

const CATALOG_KEY: &str = "catalog:apps";

fn fetch_catalog(client: &Arc<SteamClient>) -> Result<Fetched, FetchError> {
    let to_fetch = |e: SteamError| FetchError {
        message: e.message,
        status: e.status,
        transient: matches!(e.kind, ErrorKind::Network | ErrorKind::RateLimited | ErrorKind::Upstream),
    };
    let mut apps: Vec<Value> = Vec::new();
    let source;
    if client.has_key(KeyKind::Public) {
        // IStoreService/GetAppList is the maintained, paged catalog endpoint.
        source = "IStoreService/GetAppList";
        let mut last_appid: u64 = 0;
        for _page in 0..40 {
            let call = Call::get(Host::Public, "/IStoreService/GetAppList/v1/", KeyKind::Public, "static")
                .param("include_games", "true")
                .param("include_dlc", "true")
                .param("include_software", "true")
                .param("include_videos", "false")
                .param("include_hardware", "true")
                .param("max_results", 50_000)
                .opt_param("last_appid", (last_appid > 0).then_some(last_appid));
            let v = client.call_uncached(&call).map_err(to_fetch)?;
            let resp = &v["response"];
            if let Some(list) = resp["apps"].as_array() {
                for a in list {
                    if let (Some(id), Some(name)) = (a["appid"].as_u64(), a["name"].as_str()) {
                        apps.push(crate::arr![id, name]);
                    }
                }
            }
            match (resp["have_more_results"].as_bool(), resp["last_appid"].as_u64()) {
                (Some(true), Some(next)) if next > last_appid => last_appid = next,
                _ => break,
            }
        }
    } else {
        source = "ISteamApps/GetAppList";
        let call = Call::get(Host::Public, "/ISteamApps/GetAppList/v2/", KeyKind::None, "static");
        let v = client.call_uncached(&call).map_err(to_fetch)?;
        if let Some(list) = v.at(&["applist", "apps"]).as_array() {
            for a in list {
                if let (Some(id), Some(name)) = (a["appid"].as_u64(), a["name"].as_str()) {
                    if !name.trim().is_empty() {
                        apps.push(crate::arr![id, name]);
                    }
                }
            }
        }
    }
    if apps.is_empty() {
        return Err(FetchError { message: "Steam returned an empty app catalog".into(), status: Some(502), transient: true });
    }
    Ok(Fetched::Value(crate::obj! {
        "source" => source,
        "count" => apps.len(),
        "fetchedAt" => crate::util::time::iso8601_ms(crate::util::time::now_ms()),
        "apps" => apps,
    }))
}

impl SteamClient {
    /// Returns the (cached) catalog lookup and its search index.
    pub fn catalog(self: &Arc<Self>, force_refresh: bool) -> Result<(Lookup, Arc<Vec<CatalogEntry>>), SteamError> {
        let cfg = self.config();
        let policy = cfg.policy("static");
        let this = self.clone();
        let fetch: FetchFn = Arc::new(move || fetch_catalog(&this));
        let scope = cfg.public_key().map(|k| k.fingerprint()).unwrap_or_else(|| "-".into());
        let lookup = self
            .cache
            .get_or_fetch(&format!("{CATALOG_KEY}|public:{scope}"), &policy, force_refresh, fetch)
            .map_err(SteamError::from_fetch)?;
        let index = self.catalog.index_for(&lookup);
        Ok((lookup, index))
    }

    /// Catalog index only if already cached (never blocks on upstream).
    pub fn catalog_if_cached(self: &Arc<Self>) -> Option<Arc<Vec<CatalogEntry>>> {
        let cfg = self.config();
        let scope = cfg.public_key().map(|k| k.fingerprint()).unwrap_or_else(|| "-".into());
        let lookup = self.cache.peek(&format!("{CATALOG_KEY}|public:{scope}"))?;
        Some(self.catalog.index_for(&lookup))
    }
}

impl CatalogIndex {
    fn index_for(&self, lookup: &Lookup) -> Arc<Vec<CatalogEntry>> {
        let mut built = self.built.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((hash, idx)) = built.as_ref() {
            if *hash == lookup.hash {
                return idx.clone();
            }
        }
        let mut entries: Vec<CatalogEntry> = lookup.value["apps"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|pair| {
                        let id = pair[0].as_u64()?;
                        let name = pair[1].as_str()?.to_string();
                        Some(CatalogEntry { appid: id, norm: normalize(&name), name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|e| e.appid);
        entries.dedup_by_key(|e| e.appid);
        let idx = Arc::new(entries);
        *built = Some((lookup.hash.clone(), idx.clone()));
        idx
    }
}

/// Ranks catalog entries against a query. Exact > prefix > word-prefix >
/// substring > all-terms; ties broken by shorter name then lower appid.
pub fn search(index: &[CatalogEntry], query: &str, limit: usize) -> Vec<(CatalogEntry, u32)> {
    let q = normalize(query);
    if q.is_empty() {
        return Vec::new();
    }
    if let Ok(id) = query.trim().parse::<u64>() {
        if let Ok(pos) = index.binary_search_by_key(&id, |e| e.appid) {
            return vec![(index[pos].clone(), 1000)];
        }
    }
    let terms: Vec<&str> = q.split(' ').collect();
    let word_prefix = format!(" {q}");
    let mut scored: Vec<(u32, &CatalogEntry)> = Vec::new();
    for e in index {
        let score = if e.norm == q {
            100
        } else if e.norm.starts_with(&q) {
            80
        } else if e.norm.contains(&word_prefix) {
            65
        } else if e.norm.contains(&q) {
            45
        } else if terms.len() > 1 && terms.iter().all(|t| e.norm.contains(t)) {
            30
        } else {
            continue;
        };
        scored.push((score, e));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())).then(a.1.appid.cmp(&b.1.appid)));
    scored.into_iter().take(limit).map(|(s, e)| (e.clone(), s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, name: &str) -> CatalogEntry {
        CatalogEntry { appid: id, name: name.into(), norm: normalize(name) }
    }

    #[test]
    fn ranking() {
        let idx = vec![
            entry(10, "Counter-Strike"),
            entry(570, "Dota 2"),
            entry(730, "Counter-Strike 2"),
            entry(1000, "Dota Underlords"),
            entry(2000, "The Dota Story"),
        ];
        let r = search(&idx, "dota", 10);
        assert_eq!(r[0].0.appid, 570);
        assert_eq!(r.last().unwrap().0.appid, 2000);
        let r = search(&idx, "counter strike", 10);
        assert_eq!(r[0].0.appid, 10);
        assert_eq!(r[1].0.appid, 730);
        let r = search(&idx, "730", 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0.name, "Counter-Strike 2");
        assert!(search(&idx, "  ", 10).is_empty());
        assert_eq!(normalize("Half-Life: Alyx™"), "half life alyx");
    }
}
