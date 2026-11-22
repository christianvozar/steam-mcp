//! Gated partner writes: Inventory Service grants and consumption,
//! microtransactions and recurring agreements, user stats and achievements,
//! and workshop moderation. All go through the shared gate in
//! [`super::writes`] (ADR 0014): hidden unless writes are enabled,
//! confirmed through form elicitation, uncached, audit-logged, and followed by
//! a purge of the reads they affect.

use super::writes::{WriteOp, def, need_u64, obj_map, param_match, run_write, steamid};
use super::*;
use crate::steam::{Call, Host, KeyKind};

fn pcall(path: String) -> Call {
    Call::post(Host::Partner, &path, KeyKind::Publisher, "volatile")
}

/// Purge entries for `path_fragment` whose query has `name=value`.
fn purge(path_fragment: &str, name: &str, value: &str) -> Vec<Vec<String>> {
    param_match(name, value)
        .into_iter()
        .map(|m| vec![path_fragment.to_string(), m])
        .collect()
}

/// Purge entries for `path_fragment` with any parameter equal to `value`.
fn purge_value(path_fragment: &str, value: &str) -> Vec<Vec<String>> {
    [format!("={value}&"), format!("={value}|")]
        .into_iter()
        .map(|m| vec![path_fragment.to_string(), m])
        .collect()
}

fn need_str(args: &Map, name: &str) -> Result<String, ToolOutcome> {
    arg_str(args, name)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .ok_or_else(|| tool_error(format!("'{name}' is required.")))
}

// ---------------------------------------------------------------------------
// Inventory Service
// ---------------------------------------------------------------------------

fn inventory_purge(steamid: u64) -> Vec<Vec<String>> {
    let id = steamid.to_string();
    let mut v = purge("/IInventoryService/GetInventory/", "steamid", &id);
    v.extend(purge("/IInventoryService/GetQuantity/", "steamid", &id));
    v.extend(purge("/IGameInventory/GetUserHistory/", "steamid", &id));
    v
}

fn add_items(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let defs: Vec<u64> = arg_list(args, "itemdefids")
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        if defs.is_empty() {
            return Err(tool_error(
                "'itemdefids' must list at least one item definition ID.",
            ));
        }
        let mut call = pcall("/IInventoryService/AddItem/v1/".into())
            .param("appid", appid)
            .param("steamid", sid)
            .param("notify", arg_bool(args, "notify", true));
        for (i, d) in defs.iter().enumerate() {
            call = call.param(&format!("itemdefid[{i}]"), d);
        }
        call = call.opt_param(
            "requestid",
            arg_u64(args, "requestid").map(|r| r.to_string()).as_deref(),
        );
        let list = defs
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(WriteOp {
            action: "inventory items granted",
            prompt: format!("Grant item definition(s) {list} to {sid} in app {appid}?"),
            token: sid.to_string(),
            token_label: "SteamID",
            call,
            invalidate: inventory_purge(sid),
            secret_fields: &[],
            done: format!("Granted {} item(s) to {sid}.", defs.len()),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                (
                    "itemdefids",
                    defs.iter()
                        .map(|d| Value::from(*d))
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ]),
        })
    })();
    run_write(env, "steam_partner_add_inventory_items", args, op)
}

fn add_promo_item(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let def_id = need_u64(args, "itemdefid")?;
        Ok(WriteOp {
            action: "promo item granted",
            prompt: format!(
                "Grant promo item {def_id} to {sid} in app {appid}, if they are eligible?"
            ),
            token: sid.to_string(),
            token_label: "SteamID",
            call: pcall("/IInventoryService/AddPromoItem/v1/".into())
                .param("appid", appid)
                .param("steamid", sid)
                .param("itemdefid", def_id)
                .param("notify", arg_bool(args, "notify", true)),
            invalidate: inventory_purge(sid),
            secret_fields: &[],
            done: format!("Promo item {def_id} granted to {sid} (if eligible)."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                ("itemdefid", def_id.into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_add_promo_item", args, op)
}

fn consume_item(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let item = need_u64(args, "itemid")?;
        let qty = arg_u64(args, "quantity").unwrap_or(1);
        Ok(WriteOp {
            action: "inventory item consumed",
            prompt: format!(
                "Consume {qty} of item {item} from {sid}'s inventory in app {appid}? This cannot be undone."
            ),
            token: item.to_string(),
            token_label: "item ID",
            call: pcall("/IInventoryService/ConsumeItem/v1/".into())
                .param("appid", appid)
                .param("steamid", sid)
                .param("itemid", item)
                .param("quantity", qty)
                .opt_param(
                    "requestid",
                    arg_u64(args, "requestid").map(|r| r.to_string()).as_deref(),
                ),
            invalidate: inventory_purge(sid),
            secret_fields: &[],
            done: format!("Consumed {qty} of item {item} from {sid}."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                ("itemid", item.to_string().into()),
                ("quantity", qty.into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_consume_inventory_item", args, op)
}

fn modify_items(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let updates = args
            .get("updates")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if updates.is_empty() {
            return Err(tool_error(
                "'updates' must contain at least one property change.",
            ));
        }
        let mut shaped = Vec::new();
        for u in &updates {
            let item = u["itemid"]
                .as_u64_lenient()
                .ok_or_else(|| tool_error("Each update needs an 'itemid'."))?;
            let prop = u["property"]
                .as_str()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| tool_error("Each update needs a 'property' name."))?;
            let mut o = obj! { "itemid" => item.to_string(), "property_name" => prop };
            match &u["value"] {
                Value::Null if u["remove"].as_bool() == Some(true) => {
                    o.set("remove_property", true)
                }
                Value::Bool(b) => o.set("property_value_bool", *b),
                Value::String(s) => o.set("property_value_string", s.as_str()),
                Value::Number(_) if u["value"].as_i64().is_some() => {
                    o.set("property_value_int", u["value"].as_i64().unwrap_or(0))
                }
                Value::Number(_) => {
                    o.set("property_value_float", u["value"].as_f64().unwrap_or(0.0))
                }
                _ => {
                    return Err(tool_error(format!(
                        "Update for '{prop}' needs a string, number or boolean 'value', or 'remove': true."
                    )));
                }
            }
            shaped.push(o);
        }
        let input = obj! { "appid" => appid, "steamid" => sid.to_string(), "updates" => shaped, "timestamp" => crate::util::time::now_secs() };
        Ok(WriteOp {
            action: "inventory item properties modified",
            prompt: format!(
                "Change {} dynamic propert{} on {sid}'s items in app {appid}?",
                updates.len(),
                if updates.len() == 1 { "y" } else { "ies" }
            ),
            token: sid.to_string(),
            token_label: "SteamID",
            call: pcall("/IInventoryService/ModifyItems/v1/".into())
                .param("input_json", input.canonical().to_string()),
            invalidate: inventory_purge(sid),
            secret_fields: &[],
            done: format!("Applied {} property change(s) for {sid}.", updates.len()),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                ("updates", (updates.len() as u64).into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_modify_inventory_items", args, op)
}

// ---------------------------------------------------------------------------
// Microtransactions
// ---------------------------------------------------------------------------

fn mtx_iface(args: &Map) -> Result<(&'static str, &'static str), ToolOutcome> {
    match args.get("sandbox").and_then(|v| v.as_bool()) {
        Some(true) => Ok(("ISteamMicroTxnSandbox", "SANDBOX")),
        Some(false) => Ok(("ISteamMicroTxn", "LIVE (real money)")),
        None => Err(tool_error(
            "'sandbox' is required: true for ISteamMicroTxnSandbox, false for live transactions.",
        )),
    }
}

fn mtx_purge(orderid: Option<&str>, steamid: Option<u64>) -> Vec<Vec<String>> {
    let mut v = Vec::new();
    if let Some(o) = orderid {
        v.extend(purge("/QueryTxn/", "orderid", o));
    }
    if let Some(s) = steamid {
        v.extend(purge("/GetUserAgreementInfo/", "steamid", &s.to_string()));
    }
    v.push(vec!["/ISteamMicroTxn/GetReport/".into()]);
    v.push(vec!["/ISteamMicroTxnSandbox/GetReport/".into()]);
    v
}

fn init_txn(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let (iface, mode) = mtx_iface(args)?;
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let orderid = need_u64(args, "orderid")?;
        let currency = need_str(args, "currency")?.to_ascii_uppercase();
        let items = args
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() || items.len() > 100 {
            return Err(tool_error("'items' must contain 1–100 line items."));
        }
        let session = arg_str(args, "usersession").unwrap_or("client");
        let ip = arg_str(args, "ipaddress");
        if session == "web" && ip.is_none() {
            return Err(tool_error(
                "'ipaddress' is required when usersession is 'web'.",
            ));
        }
        let mut call = pcall(format!("/{iface}/InitTxn/v3/"))
            .param("orderid", orderid)
            .param("steamid", sid)
            .param("appid", appid)
            .param("itemcount", items.len())
            .param("language", arg_str(args, "language").unwrap_or("en"))
            .param("currency", &currency)
            .param("usersession", session)
            .opt_param("ipaddress", ip);
        let mut total: i64 = 0;
        let mut lines = Vec::new();
        for (i, it) in items.iter().enumerate() {
            let itemid = it["itemid"]
                .as_u64_lenient()
                .ok_or_else(|| tool_error(format!("items[{i}] needs 'itemid'.")))?;
            let qty = it["qty"].as_u64_lenient().unwrap_or(1);
            let amount = it["amount"].as_i64().filter(|a| *a >= 0).ok_or_else(|| tool_error(format!("items[{i}] needs a non-negative integer 'amount' (total in cents for the line).")))?;
            let desc = it["description"]
                .as_str()
                .filter(|d| !d.is_empty())
                .ok_or_else(|| tool_error(format!("items[{i}] needs a 'description'.")))?;
            total += amount;
            lines.push(format!("{qty}× {desc} ({amount})"));
            call = call
                .param(&format!("itemid[{i}]"), itemid)
                .param(&format!("qty[{i}]"), qty)
                .param(&format!("amount[{i}]"), amount)
                .param(&format!("description[{i}]"), desc)
                .opt_param(&format!("category[{i}]"), it["category"].as_str());
        }
        Ok(WriteOp {
            action: "microtransaction initiated",
            prompt: format!(
                "Start a {mode} transaction, order {orderid}, for {sid} in app {appid}: {} — total {total} {currency} (smallest currency unit)? The user must still approve it in Steam.",
                lines.join(", ")
            ),
            token: orderid.to_string(),
            token_label: "order ID",
            call,
            invalidate: mtx_purge(Some(&orderid.to_string()), Some(sid)),
            secret_fields: &[],
            done: format!(
                "Order {orderid} is waiting for {sid} to approve it in Steam; finalize it with steam_partner_finalize_microtxn after approval."
            ),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                ("orderid", orderid.to_string().into()),
                ("sandbox", (iface.ends_with("Sandbox")).into()),
                ("total", total.into()),
                ("currency", currency.as_str().into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_init_microtxn", args, op)
}

fn order_op(
    env: &ToolEnv,
    args: &Map,
    tool: &str,
    method: &str,
    verb: &'static str,
    warn: &str,
) -> ToolOutcome {
    let op = (|| {
        let (iface, mode) = mtx_iface(args)?;
        let appid = need_u64(args, "appid")?;
        let orderid = need_u64(args, "orderid")?;
        Ok(WriteOp {
            action: verb,
            prompt: format!("{warn} {mode} order {orderid} in app {appid}?"),
            token: orderid.to_string(),
            token_label: "order ID",
            call: pcall(format!("/{iface}/{method}/v2/"))
                .param("orderid", orderid)
                .param("appid", appid),
            invalidate: mtx_purge(Some(&orderid.to_string()), None),
            secret_fields: &[],
            done: format!("Order {orderid}: {verb}."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("orderid", orderid.to_string().into()),
                ("sandbox", iface.ends_with("Sandbox").into()),
            ]),
        })
    })();
    run_write(env, tool, args, op)
}

fn finalize_txn(env: &ToolEnv, args: &Map) -> ToolOutcome {
    order_op(
        env,
        args,
        "steam_partner_finalize_microtxn",
        "FinalizeTxn",
        "microtransaction finalized",
        "Charge the user and complete",
    )
}

fn refund_txn(env: &ToolEnv, args: &Map) -> ToolOutcome {
    order_op(
        env,
        args,
        "steam_partner_refund_microtxn",
        "RefundTxn",
        "microtransaction refunded",
        "Refund",
    )
}

fn agreement_op(env: &ToolEnv, args: &Map, kind: &str) -> ToolOutcome {
    let tool = match kind {
        "cancel" => "steam_partner_cancel_agreement",
        "adjust" => "steam_partner_adjust_agreement",
        _ => "steam_partner_process_agreement",
    };
    let op = (|| {
        let (iface, mode) = mtx_iface(args)?;
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let agreement = need_u64(args, "agreementid")?;
        let base = |m: &str| {
            pcall(format!("/{iface}/{m}/v1/"))
                .param("steamid", sid)
                .param("agreementid", agreement)
                .param("appid", appid)
        };
        let (action, prompt, call, orderid) = match kind {
            "cancel" => (
                "recurring agreement cancelled",
                format!(
                    "Cancel {mode} recurring agreement {agreement} for {sid} in app {appid}? No further charges will be made."
                ),
                base("CancelAgreement"),
                None,
            ),
            "adjust" => {
                let date = need_str(args, "next_process_date")?;
                if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(tool_error("'next_process_date' must be YYYYMMDD."));
                }
                (
                    "recurring agreement adjusted",
                    format!(
                        "Move the next billing date of {mode} agreement {agreement} for {sid} to {date}?"
                    ),
                    base("AdjustAgreement").param("nextprocessdate", &date),
                    None,
                )
            }
            _ => {
                let orderid = need_u64(args, "orderid")?;
                let amount = need_u64(args, "amount")?;
                let currency = need_str(args, "currency")?.to_ascii_uppercase();
                (
                    "recurring agreement charged",
                    format!(
                        "Charge {mode} agreement {agreement} for {sid} now: {amount} {currency} (smallest currency unit), order {orderid}?"
                    ),
                    base("ProcessAgreement")
                        .param("orderid", orderid)
                        .param("amount", amount)
                        .param("currency", &currency),
                    Some(orderid.to_string()),
                )
            }
        };
        Ok(WriteOp {
            action,
            prompt,
            token: agreement.to_string(),
            token_label: "agreement ID",
            call,
            invalidate: mtx_purge(orderid.as_deref(), Some(sid)),
            secret_fields: &[],
            done: format!("Agreement {agreement}: {action}."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                ("agreementid", agreement.to_string().into()),
                ("sandbox", iface.ends_with("Sandbox").into()),
            ]),
        })
    })();
    run_write(env, tool, args, op)
}

fn cancel_agreement(env: &ToolEnv, args: &Map) -> ToolOutcome {
    agreement_op(env, args, "cancel")
}
fn adjust_agreement(env: &ToolEnv, args: &Map) -> ToolOutcome {
    agreement_op(env, args, "adjust")
}
fn process_agreement(env: &ToolEnv, args: &Map) -> ToolOutcome {
    agreement_op(env, args, "process")
}

// ---------------------------------------------------------------------------
// User stats and achievements
// ---------------------------------------------------------------------------

fn set_user_stats(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let mut pairs: Vec<(String, i64)> = Vec::new();
        if let Some(stats) = args.get("stats").and_then(|v| v.as_object()) {
            for (k, v) in stats.iter() {
                let n = v
                    .as_i64()
                    .ok_or_else(|| tool_error(format!("Stat '{k}' must be an integer.")))?;
                pairs.push((k.to_string(), n));
            }
        }
        if let Some(ach) = args.get("achievements").and_then(|v| v.as_object()) {
            for (k, v) in ach.iter() {
                let b = v.as_bool().ok_or_else(|| {
                    tool_error(format!(
                        "Achievement '{k}' must be true (unlock) or false (clear)."
                    ))
                })?;
                pairs.push((k.to_string(), b as i64));
            }
        }
        if pairs.is_empty() || pairs.len() > 100 {
            return Err(tool_error(
                "Provide 1–100 entries across 'stats' and 'achievements'.",
            ));
        }
        let mut call = pcall("/ISteamUserStats/SetUserStatsForGame/v1/".into())
            .param("steamid", sid)
            .param("appid", appid)
            .param("count", pairs.len());
        for (i, (k, v)) in pairs.iter().enumerate() {
            call = call
                .param(&format!("name[{i}]"), k)
                .param(&format!("value[{i}]"), v);
        }
        let summary = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let id = sid.to_string();
        let mut invalidate = Vec::new();
        for path in [
            "/ISteamUserStats/GetUserStatsForGame/",
            "/ISteamUserStats/GetPlayerAchievements/",
        ] {
            for m in param_match("steamid", &id) {
                invalidate.push(vec![path.to_string(), m, format!("appid={appid}")]);
            }
        }
        Ok(WriteOp {
            action: "user stats set",
            prompt: format!(
                "Set these stats/achievements for {sid} in app {appid}: {}?",
                crate::util::truncate(&summary, 600)
            ),
            token: sid.to_string(),
            token_label: "SteamID",
            call,
            invalidate,
            secret_fields: &[],
            done: format!("Set {} stat/achievement value(s) for {sid}.", pairs.len()),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", id.as_str().into()),
                (
                    "names",
                    pairs
                        .iter()
                        .map(|(k, _)| Value::from(k.as_str()))
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ]),
        })
    })();
    run_write(env, "steam_partner_set_user_stats", args, op)
}

// ---------------------------------------------------------------------------
// Workshop moderation
// ---------------------------------------------------------------------------

fn workshop_purge(fileid: u64) -> Vec<Vec<String>> {
    let id = fileid.to_string();
    let mut v = purge_value("/IPublishedFileService/GetDetails/", &id);
    v.extend(purge_value(
        "/ISteamRemoteStorage/GetPublishedFileDetails/",
        &id,
    ));
    v.extend(purge_value(
        "/IPublishedFileService/GetSubSectionData/",
        &id,
    ));
    v.push(vec!["/IPublishedFileService/QueryFiles/".into()]);
    v
}

fn workshop_op(env: &ToolEnv, args: &Map, kind: &str) -> ToolOutcome {
    let tool = match kind {
        "metadata" => "steam_partner_set_workshop_metadata",
        "tags" => "steam_partner_update_workshop_tags",
        "ban" => "steam_partner_set_workshop_ban",
        _ => "steam_partner_set_workshop_incompatible",
    };
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let file = need_u64(args, "publishedfileid")?;
        let base = |m: &str| {
            pcall(format!("/IPublishedFileService/{m}/v1/"))
                .param("publishedfileid", file)
                .param("appid", appid)
        };
        let (action, prompt, call) = match kind {
            "metadata" => {
                let md = arg_str(args, "metadata").unwrap_or_default().to_string();
                (
                    "workshop metadata set",
                    format!(
                        "Replace the developer metadata of workshop item {file} ({} bytes)?",
                        md.len()
                    ),
                    base("SetDeveloperMetadata").param("metadata", &md),
                )
            }
            "tags" => {
                let add = arg_list(args, "add_tags");
                let remove = arg_list(args, "remove_tags");
                if add.is_empty() && remove.is_empty() {
                    return Err(tool_error("Provide 'add_tags' and/or 'remove_tags'."));
                }
                let mut c = base("UpdateTags");
                for (i, t) in add.iter().enumerate() {
                    c = c.param(&format!("add_tags[{i}]"), t);
                }
                for (i, t) in remove.iter().enumerate() {
                    c = c.param(&format!("remove_tags[{i}]"), t);
                }
                (
                    "workshop tags updated",
                    format!(
                        "On workshop item {file}, add tags [{}] and remove tags [{}]?",
                        add.join(", "),
                        remove.join(", ")
                    ),
                    c,
                )
            }
            "ban" => {
                let banned = arg_bool(args, "banned", true);
                let reason = arg_str(args, "reason").unwrap_or_default().to_string();
                (
                    "workshop ban status set",
                    format!(
                        "{} workshop item {file} in app {appid}{}?",
                        if banned { "Ban" } else { "Unban" },
                        if reason.is_empty() {
                            String::new()
                        } else {
                            format!(" (reason: {reason})")
                        }
                    ),
                    base("UpdateBanStatus")
                        .param("banned", banned)
                        .param("reason", &reason),
                )
            }
            _ => {
                let inc = arg_bool(args, "incompatible", true);
                (
                    "workshop compatibility set",
                    format!(
                        "Mark workshop item {file} as {}?",
                        if inc { "incompatible" } else { "compatible" }
                    ),
                    base("UpdateIncompatibleStatus").param("incompatible", inc),
                )
            }
        };
        Ok(WriteOp {
            action,
            prompt,
            token: file.to_string(),
            token_label: "workshop item ID",
            call,
            invalidate: workshop_purge(file),
            secret_fields: &[],
            done: format!("Workshop item {file}: {action}."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("publishedfileid", file.to_string().into()),
            ]),
        })
    })();
    run_write(env, tool, args, op)
}

fn set_workshop_metadata(env: &ToolEnv, args: &Map) -> ToolOutcome {
    workshop_op(env, args, "metadata")
}
fn update_workshop_tags(env: &ToolEnv, args: &Map) -> ToolOutcome {
    workshop_op(env, args, "tags")
}
fn set_workshop_ban(env: &ToolEnv, args: &Map) -> ToolOutcome {
    workshop_op(env, args, "ban")
}
fn set_workshop_incompatible(env: &ToolEnv, args: &Map) -> ToolOutcome {
    workshop_op(env, args, "incompatible")
}

fn ugc_ban(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let sid = steamid(args, "steamid")?;
        let until = arg_u64(args, "expiration_time").unwrap_or(0);
        let reason = arg_str(args, "reason").unwrap_or_default().to_string();
        let span = if until == 0 {
            "permanently".to_string()
        } else {
            format!("until {}", crate::util::time::iso8601_secs(until))
        };
        Ok(WriteOp {
            action: "user UGC ban set",
            prompt: format!("Ban {sid} from posting workshop content in app {appid} {span}?"),
            token: sid.to_string(),
            token_label: "SteamID",
            call: pcall("/IPublishedFileService/UpdateAppUGCBan/v1/".into())
                .param("steamid", sid)
                .param("appid", appid)
                .param("expiration_time", until)
                .param("reason", &reason),
            invalidate: Vec::new(),
            secret_fields: &[],
            done: format!("{sid} is banned from posting content in app {appid} {span}."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("steamid", sid.to_string().into()),
                ("expirationTime", until.into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_ban_user_ugc", args, op)
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn sandbox() -> Value {
    obj! { "type" => "boolean", "description" => "true for ISteamMicroTxnSandbox, false for live (real money). Required, so a live charge is never the silent default." }
}

fn currency() -> Value {
    obj! { "type" => "string", "pattern" => "^[A-Za-z]{3}$", "description" => "ISO 4217 currency code, e.g. USD." }
}

pub fn tools() -> Vec<ToolDef> {
    let pk = KeyKind::Publisher;
    let ids = |d: &str| obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 1, "maxItems" => 100, "description" => d };
    let line_item = obj! {
        "type" => "object",
        "properties" => obj! {
            "itemid" => obj! { "type" => "integer", "minimum" => 0, "description" => "Your item ID." },
            "qty" => obj! { "type" => "integer", "minimum" => 1, "maximum" => 32767, "default" => 1 },
            "amount" => obj! { "type" => "integer", "minimum" => 0, "description" => "Line total in the smallest currency unit (e.g. cents)." },
            "description" => obj! { "type" => "string", "minLength" => 1, "maxLength" => 128 },
            "category" => obj! { "type" => "string", "maxLength" => 64 },
        },
        "required" => crate::arr!["itemid", "amount", "description"],
    };
    let update = obj! {
        "type" => "object",
        "properties" => obj! {
            "itemid" => obj! { "type" => ["integer", "string"], "description" => "Item instance ID." },
            "property" => obj! { "type" => "string", "minLength" => 1 },
            "value" => obj! { "type" => ["string", "number", "boolean"] },
            "remove" => obj! { "type" => "boolean", "description" => "Remove the property instead of setting it." },
        },
        "required" => crate::arr!["itemid", "property"],
    };
    vec![
        def(
            "steam_partner_add_inventory_items",
            "Grant Inventory Items",
            "Grants items to a user through the Inventory Service (AddItem).",
            pk,
            false,
            false,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("itemdefids", ids("Item definition IDs, one item each.")),
                    ("notify", p_bool("Notify the user.", true)),
                    (
                        "requestid",
                        p_u64("Idempotency key; a repeated request ID is not granted twice."),
                    ),
                ],
                &["appid", "steamid", "itemdefids"],
            ),
            add_items,
        ),
        def(
            "steam_partner_add_promo_item",
            "Grant Promo Item",
            "Grants a promo item if the user is eligible (AddPromoItem).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("itemdefid", p_u64("Promo item definition ID.")),
                    ("notify", p_bool("Notify the user.", true)),
                ],
                &["appid", "steamid", "itemdefid"],
            ),
            add_promo_item,
        ),
        def(
            "steam_partner_consume_inventory_item",
            "Consume Inventory Item",
            "Consumes (removes) a quantity of an item from a user's inventory (ConsumeItem).",
            pk,
            true,
            false,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("itemid", p_u64("Item instance ID.")),
                    ("quantity", p_int("Quantity.", 1, 65535, Some(1))),
                    ("requestid", p_u64("Idempotency key.")),
                ],
                &["appid", "steamid", "itemid"],
            ),
            consume_item,
        ),
        def(
            "steam_partner_modify_inventory_items",
            "Modify Item Properties",
            "Sets or removes dynamic properties on a user's items (ModifyItems).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    (
                        "updates",
                        obj! { "type" => "array", "items" => update, "minItems" => 1, "maxItems" => 100 },
                    ),
                ],
                &["appid", "steamid", "updates"],
            ),
            modify_items,
        ),
        def(
            "steam_partner_init_microtxn",
            "Start Microtransaction",
            "Starts a microtransaction the user must approve in Steam (InitTxn v3).",
            pk,
            false,
            false,
            object_schema(
                vec![
                    ("sandbox", sandbox()),
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("orderid", p_u64("Your unique 64-bit order ID.")),
                    ("currency", currency()),
                    (
                        "items",
                        obj! { "type" => "array", "items" => line_item, "minItems" => 1, "maxItems" => 100 },
                    ),
                    (
                        "language",
                        p_str("ISO 639-1 language for descriptions (default en)."),
                    ),
                    (
                        "usersession",
                        p_enum(
                            "Where the user approves.",
                            &["client", "web"],
                            Some("client"),
                        ),
                    ),
                    (
                        "ipaddress",
                        p_str("User IP address (required for web sessions)."),
                    ),
                ],
                &[
                    "sandbox", "appid", "steamid", "orderid", "currency", "items",
                ],
            ),
            init_txn,
        ),
        def(
            "steam_partner_finalize_microtxn",
            "Finalize Microtransaction",
            "Completes an approved microtransaction and charges the user (FinalizeTxn).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("sandbox", sandbox()),
                    ("appid", p_appid()),
                    ("orderid", p_u64("Order ID.")),
                ],
                &["sandbox", "appid", "orderid"],
            ),
            finalize_txn,
        ),
        def(
            "steam_partner_refund_microtxn",
            "Refund Microtransaction",
            "Refunds a completed microtransaction (RefundTxn).",
            pk,
            true,
            true,
            object_schema(
                vec![
                    ("sandbox", sandbox()),
                    ("appid", p_appid()),
                    ("orderid", p_u64("Order ID.")),
                ],
                &["sandbox", "appid", "orderid"],
            ),
            refund_txn,
        ),
        def(
            "steam_partner_cancel_agreement",
            "Cancel Recurring Agreement",
            "Cancels a recurring billing agreement (CancelAgreement).",
            pk,
            true,
            true,
            object_schema(
                vec![
                    ("sandbox", sandbox()),
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("agreementid", p_u64("Agreement ID.")),
                ],
                &["sandbox", "appid", "steamid", "agreementid"],
            ),
            cancel_agreement,
        ),
        def(
            "steam_partner_adjust_agreement",
            "Adjust Recurring Agreement",
            "Moves the next billing date of a recurring agreement (AdjustAgreement).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("sandbox", sandbox()),
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("agreementid", p_u64("Agreement ID.")),
                    (
                        "next_process_date",
                        obj! { "type" => "string", "pattern" => "^[0-9]{8}$", "description" => "Next billing date, YYYYMMDD." },
                    ),
                ],
                &[
                    "sandbox",
                    "appid",
                    "steamid",
                    "agreementid",
                    "next_process_date",
                ],
            ),
            adjust_agreement,
        ),
        def(
            "steam_partner_process_agreement",
            "Charge Recurring Agreement",
            "Charges a recurring agreement now, for agreements billed by the game (ProcessAgreement).",
            pk,
            false,
            false,
            object_schema(
                vec![
                    ("sandbox", sandbox()),
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("agreementid", p_u64("Agreement ID.")),
                    ("orderid", p_u64("New unique order ID.")),
                    ("amount", p_u64("Amount in the smallest currency unit.")),
                    ("currency", currency()),
                ],
                &[
                    "sandbox",
                    "appid",
                    "steamid",
                    "agreementid",
                    "orderid",
                    "amount",
                    "currency",
                ],
            ),
            process_agreement,
        ),
        def(
            "steam_partner_set_user_stats",
            "Set User Stats",
            "Sets a user's integer stats and unlocks or clears achievements (SetUserStatsForGame).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    (
                        "stats",
                        obj! { "type" => "object", "additionalProperties" => obj! { "type" => "integer" }, "description" => "Stat API name → integer value." },
                    ),
                    (
                        "achievements",
                        obj! { "type" => "object", "additionalProperties" => obj! { "type" => "boolean" }, "description" => "Achievement API name → true (unlock) or false (clear)." },
                    ),
                ],
                &["appid", "steamid"],
            ),
            set_user_stats,
        ),
        def(
            "steam_partner_set_workshop_metadata",
            "Set Workshop Metadata",
            "Replaces a workshop item's developer metadata (SetDeveloperMetadata).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("publishedfileid", p_u64("Workshop item ID.")),
                    (
                        "metadata",
                        obj! { "type" => "string", "maxLength" => 5000, "description" => "Metadata string (often JSON)." },
                    ),
                ],
                &["appid", "publishedfileid", "metadata"],
            ),
            set_workshop_metadata,
        ),
        def(
            "steam_partner_update_workshop_tags",
            "Update Workshop Tags",
            "Adds and removes tags on a workshop item (UpdateTags).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("publishedfileid", p_u64("Workshop item ID.")),
                    (
                        "add_tags",
                        obj! { "type" => "array", "items" => obj! { "type" => "string", "minLength" => 1 }, "maxItems" => 50 },
                    ),
                    (
                        "remove_tags",
                        obj! { "type" => "array", "items" => obj! { "type" => "string", "minLength" => 1 }, "maxItems" => 50 },
                    ),
                ],
                &["appid", "publishedfileid"],
            ),
            update_workshop_tags,
        ),
        def(
            "steam_partner_set_workshop_ban",
            "Ban Workshop Item",
            "Bans or unbans a workshop item (UpdateBanStatus).",
            pk,
            true,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("publishedfileid", p_u64("Workshop item ID.")),
                    ("banned", p_bool("Ban (true) or unban (false).", true)),
                    ("reason", p_str("Reason shown to the author.")),
                ],
                &["appid", "publishedfileid"],
            ),
            set_workshop_ban,
        ),
        def(
            "steam_partner_set_workshop_incompatible",
            "Mark Workshop Item Incompatible",
            "Marks a workshop item incompatible or compatible (UpdateIncompatibleStatus).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("publishedfileid", p_u64("Workshop item ID.")),
                    (
                        "incompatible",
                        p_bool("Incompatible (true) or compatible (false).", true),
                    ),
                ],
                &["appid", "publishedfileid"],
            ),
            set_workshop_incompatible,
        ),
        def(
            "steam_partner_ban_user_ugc",
            "Ban User from Workshop",
            "Bans a user from posting workshop content for your app (UpdateAppUGCBan).",
            pk,
            true,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    (
                        "expiration_time",
                        p_u64("Ban end (Unix time); 0 = permanent."),
                    ),
                    ("reason", p_str("Reason shown to the user.")),
                ],
                &["appid", "steamid"],
            ),
            ugc_ban,
        ),
    ]
}
