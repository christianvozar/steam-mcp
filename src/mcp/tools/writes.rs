//! Gated state-changing tools: leaderboards and game server accounts.
//!
//! Every tool here is hidden unless `STEAM_MCP_ENABLE_WRITES=true`, requires
//! form elicitation, and only acts after the user accepts a confirmation
//! form and types the target's identifier. The sealed MRTR `requestState`
//! carries a fingerprint of the exact arguments, so a confirmation cannot
//! be replayed against a different call. Each executed write is audit-logged
//! (without secrets) and purges the cached reads it affects.

use super::*;
use crate::mcp::context::scopes;
use crate::steam::{Call, Host, KeyKind, parse_steamid};
use crate::try_steam;

/// A fully-resolved write, ready to confirm and execute.
pub(super) struct WriteOp {
    /// Short verb phrase for the audit log, e.g. "leaderboard reset".
    pub(super) action: &'static str,
    /// The question shown to the user.
    pub(super) prompt: String,
    /// What the user must type to confirm.
    pub(super) token: String,
    /// What the token is, e.g. "leaderboard ID".
    pub(super) token_label: &'static str,
    pub(super) call: Call,
    /// Cache keys containing all of the strings in any one entry are purged.
    pub(super) invalidate: Vec<Vec<String>>,
    /// Response fields that are secrets: returned to the caller, never logged.
    pub(super) secret_fields: &'static [&'static str],
    /// Summary for a successful result.
    pub(super) done: String,
    /// Non-secret identifying fields for the audit log and the result.
    pub(super) subject: Map,
}

fn fingerprint(tool: &str, args: &Map) -> String {
    let mut a = args.clone();
    a.remove("force_refresh");
    let canon = Value::Object(a).canonical().to_string();
    crate::util::crypto::sha256_hex(format!("{tool}\n{canon}").as_bytes())[..32].to_string()
}

/// Matches `name=value` exactly inside a cache key's sorted query string.
pub(super) fn param_match(name: &str, value: &str) -> [String; 2] {
    [format!("{name}={value}&"), format!("{name}={value}|")]
}

pub(super) fn run_write(
    env: &ToolEnv,
    tool: &str,
    args: &Map,
    op: Result<WriteOp, ToolOutcome>,
) -> ToolOutcome {
    if !env.cfg().enable_writes {
        return tool_error(
            "State-changing tools are disabled. Set STEAM_MCP_ENABLE_WRITES=true to allow them.",
        );
    }
    let op = match op {
        Ok(op) => op,
        Err(o) => return o,
    };
    if !env.ctx.supports_elicitation_form() {
        return ToolOutcome::Error(McpError::missing_capability(
            obj! { "elicitation" => obj! { "form" => obj! {} } },
            "this change requires explicit user confirmation via form elicitation",
        ));
    }
    let fp = fingerprint(tool, args);
    let bound = env.mrtr.state.as_ref().is_some_and(|s| {
        s["step"].as_str() == Some("confirm") && s["op"].as_str() == Some(fp.as_str())
    });
    if let (true, Some(answer)) = (bound, env.mrtr.response("confirm")) {
        if answer["action"].as_str() != Some("accept") {
            return tool_error("The change was not confirmed by the user; nothing was changed.");
        }
        let typed = answer.at(&["content", "confirmation"]).to_param_string();
        if answer.at(&["content", "confirm"]).as_bool() == Some(true) && typed.trim() == op.token {
            return execute(env, op);
        }
        // A mismatched confirmation falls through and asks again.
    }
    let mut requests = Map::new();
    requests.insert("confirm", obj! {
        "method" => "elicitation/create",
        "params" => obj! {
            "mode" => "form",
            "message" => format!("{} Type the {} ({}) to confirm.", op.prompt, op.token_label, op.token),
            "requestedSchema" => obj! {
                "type" => "object",
                "properties" => obj! {
                    "confirm" => obj! { "type" => "boolean", "title" => "Make this change", "default" => false },
                    "confirmation" => obj! { "type" => "string", "title" => capitalize(op.token_label), "description" => format!("Type {} to confirm", op.token) },
                },
                "required" => crate::arr!["confirm", "confirmation"],
            },
        },
    });
    ToolOutcome::InputRequired {
        requests,
        state: Some(obj! { "step" => "confirm", "op" => fp }),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

fn execute(env: &ToolEnv, op: WriteOp) -> ToolOutcome {
    let v = try_steam!(env.steam().call_uncached(&op.call));
    let resp = if v["response"].is_object() {
        v["response"].clone()
    } else if v["result"].is_object() {
        v["result"].clone()
    } else {
        v.clone()
    };
    // Steam reports some failures inside a 200 response.
    let failed = resp["result"].as_u64().is_some_and(|r| r != 1)
        || resp["result"]
            .as_str()
            .is_some_and(|r| !r.eq_ignore_ascii_case("OK"))
        || resp["success"].as_bool() == Some(false)
        || resp["error"].is_string()
        || resp["error"].is_object();
    if failed {
        return tool_error(format!("Steam rejected the {}: {resp}", op.action));
    }
    let purged = env.steam().cache.invalidate_where(|k| {
        op.invalidate
            .iter()
            .any(|all| all.iter().all(|s| k.contains(s.as_str())))
    });
    let redacted: Vec<&str> = op
        .secret_fields
        .iter()
        .copied()
        .filter(|f| !resp[*f].is_null())
        .collect();
    crate::log_info!(
        "audit",
        op.action,
        subject = Value::Object(op.subject.clone()),
        principal = env.ctx.principal.id,
        secrets_returned = redacted.join(",")
    );
    let mut data = op.subject;
    data.insert("response", resp);
    data.insert("cacheEntriesRefreshed", purged as u64);
    let note = if redacted.is_empty() {
        String::new()
    } else {
        format!(
            " The response contains a secret ({}); store it securely, it is not shown again.",
            redacted.join(", ")
        )
    };
    ok_result(format!("{}{note}", op.done), Value::Object(data), &[])
}

pub(super) fn steamid(args: &Map, name: &str) -> Result<u64, ToolOutcome> {
    let raw = args
        .get(name)
        .map(|v| v.to_param_string())
        .unwrap_or_default();
    parse_steamid(&raw)
        .ok_or_else(|| tool_error(format!("'{name}' must be a 64-bit SteamID, got '{raw}'.")))
}

pub(super) fn need_u64(args: &Map, name: &str) -> Result<u64, ToolOutcome> {
    arg_u64(args, name).ok_or_else(|| tool_error(format!("'{name}' is required.")))
}

fn lb_call(path: &str) -> Call {
    Call::post(Host::Partner, path, KeyKind::Publisher, "volatile")
}

fn lb_invalidate(appid: u64, board: Option<u64>) -> Vec<Vec<String>> {
    let [a1, a2] = param_match("appid", &appid.to_string());
    let mut v = vec![
        vec![
            "/ISteamLeaderboards/GetLeaderboardsForGame/".into(),
            a1.clone(),
        ],
        vec![
            "/ISteamLeaderboards/GetLeaderboardsForGame/".into(),
            a2.clone(),
        ],
    ];
    match board {
        Some(b) => {
            for m in param_match("leaderboardid", &b.to_string()) {
                v.push(vec!["/ISteamLeaderboards/GetLeaderboardEntries/".into(), m]);
            }
        }
        None => {
            v.push(vec![
                "/ISteamLeaderboards/GetLeaderboardEntries/".into(),
                a1,
            ]);
            v.push(vec![
                "/ISteamLeaderboards/GetLeaderboardEntries/".into(),
                a2,
            ]);
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Leaderboards
// ---------------------------------------------------------------------------

fn find_or_create_leaderboard(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let name = arg_str(args, "name").unwrap_or_default().to_string();
        let sort = arg_str(args, "sort_method").unwrap_or("Descending");
        let display = arg_str(args, "display_type").unwrap_or("Numeric");
        let call = lb_call("/ISteamLeaderboards/FindOrCreateLeaderboard/v2/")
            .param("appid", appid)
            .param("name", &name)
            .param("sortmethod", sort)
            .param("displaytype", display)
            .param("createifnotfound", "true")
            .param(
                "onlytrustedwrites",
                arg_bool(args, "only_trusted_writes", false),
            )
            .param(
                "onlyfriendsreads",
                arg_bool(args, "only_friends_reads", false),
            );
        Ok(WriteOp {
            action: "leaderboard created",
            prompt: format!(
                "Create leaderboard '{name}' for app {appid} (sorted {sort}, displayed {display}) if it does not exist?"
            ),
            token: appid.to_string(),
            token_label: "app ID",
            call,
            invalidate: lb_invalidate(appid, None),
            secret_fields: &[],
            done: format!("Leaderboard '{name}' exists for app {appid}."),
            subject: obj_map(&[("appid", appid.into()), ("name", name.as_str().into())]),
        })
    })();
    run_write(env, "steam_partner_create_leaderboard", args, op)
}

fn set_leaderboard_score(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let board = need_u64(args, "leaderboardid")?;
        let sid = steamid(args, "steamid")?;
        let score = args
            .get("score")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| tool_error("'score' must be an integer."))?;
        let method = arg_str(args, "score_method").unwrap_or("KeepBest");
        let call = lb_call("/ISteamLeaderboards/SetLeaderboardScore/v1/")
            .param("appid", appid)
            .param("leaderboardid", board)
            .param("steamid", sid)
            .param("score", score)
            .param("scoremethod", method)
            .opt_param("details", arg_str(args, "details_hex"));
        Ok(WriteOp {
            action: "leaderboard score set",
            prompt: format!(
                "Set {sid}'s score on leaderboard {board} (app {appid}) to {score} using {method}?"
            ),
            token: board.to_string(),
            token_label: "leaderboard ID",
            call,
            invalidate: lb_invalidate(appid, Some(board)),
            secret_fields: &[],
            done: format!("Score {score} submitted for {sid} on leaderboard {board} ({method})."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("leaderboardid", board.into()),
                ("steamid", sid.to_string().into()),
                ("score", score.into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_set_leaderboard_score", args, op)
}

fn delete_leaderboard_score(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let board = need_u64(args, "leaderboardid")?;
        let sid = steamid(args, "steamid")?;
        Ok(WriteOp {
            action: "leaderboard score deleted",
            prompt: format!(
                "Delete {sid}'s entry from leaderboard {board} (app {appid})? This cannot be undone."
            ),
            token: sid.to_string(),
            token_label: "SteamID",
            call: lb_call("/ISteamLeaderboards/DeleteLeaderboardScore/v1/")
                .param("appid", appid)
                .param("leaderboardid", board)
                .param("steamid", sid),
            invalidate: lb_invalidate(appid, Some(board)),
            secret_fields: &[],
            done: format!("Deleted {sid}'s entry from leaderboard {board}."),
            subject: obj_map(&[
                ("appid", appid.into()),
                ("leaderboardid", board.into()),
                ("steamid", sid.to_string().into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_delete_leaderboard_score", args, op)
}

fn reset_leaderboard(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let board = need_u64(args, "leaderboardid")?;
        Ok(WriteOp {
            action: "leaderboard reset",
            prompt: format!(
                "Remove EVERY entry from leaderboard {board} (app {appid})? This cannot be undone."
            ),
            token: board.to_string(),
            token_label: "leaderboard ID",
            call: lb_call("/ISteamLeaderboards/ResetLeaderboard/v1/")
                .param("appid", appid)
                .param("leaderboardid", board),
            invalidate: lb_invalidate(appid, Some(board)),
            secret_fields: &[],
            done: format!("Leaderboard {board} was reset."),
            subject: obj_map(&[("appid", appid.into()), ("leaderboardid", board.into())]),
        })
    })();
    run_write(env, "steam_partner_reset_leaderboard", args, op)
}

fn delete_leaderboard(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let name = arg_str(args, "name").unwrap_or_default().to_string();
        Ok(WriteOp {
            action: "leaderboard deleted",
            prompt: format!(
                "Delete leaderboard '{name}' and all its entries from app {appid}? This cannot be undone."
            ),
            token: name.clone(),
            token_label: "leaderboard name",
            call: lb_call("/ISteamLeaderboards/DeleteLeaderboard/v1/")
                .param("appid", appid)
                .param("name", &name),
            invalidate: lb_invalidate(appid, None),
            secret_fields: &[],
            done: format!("Leaderboard '{name}' was deleted from app {appid}."),
            subject: obj_map(&[("appid", appid.into()), ("name", name.as_str().into())]),
        })
    })();
    run_write(env, "steam_partner_delete_leaderboard", args, op)
}

// ---------------------------------------------------------------------------
// Game server accounts (owned by the account of the configured key)
// ---------------------------------------------------------------------------

fn gs_call(path: &str) -> Call {
    Call::post(Host::Public, path, KeyKind::Public, "volatile")
}

fn gs_invalidate(server: Option<u64>) -> Vec<Vec<String>> {
    let mut v = vec![vec!["/IGameServersService/GetAccountList/".to_string()]];
    if let Some(s) = server {
        for m in param_match("steamid", &s.to_string()) {
            v.push(vec!["/IGameServersService/GetAccountPublicInfo/".into(), m]);
        }
    }
    v
}

fn create_gs_account(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let appid = need_u64(args, "appid")?;
        let memo = arg_str(args, "memo").unwrap_or_default().to_string();
        Ok(WriteOp {
            action: "game server account created",
            prompt: format!(
                "Create a persistent game server account for app {appid} with memo '{memo}'? Steam returns a login token (GSLT)."
            ),
            token: appid.to_string(),
            token_label: "app ID",
            call: gs_call("/IGameServersService/CreateAccount/v1/")
                .param("appid", appid)
                .param("memo", &memo),
            invalidate: gs_invalidate(None),
            secret_fields: &["login_token"],
            done: format!("Created a game server account for app {appid}."),
            subject: obj_map(&[("appid", appid.into()), ("memo", memo.as_str().into())]),
        })
    })();
    run_write(env, "steam_create_game_server_account", args, op)
}

fn set_gs_memo(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let sid = steamid(args, "steamid")?;
        let memo = arg_str(args, "memo").unwrap_or_default().to_string();
        Ok(WriteOp {
            action: "game server memo set",
            prompt: format!("Change the memo of game server account {sid} to '{memo}'?"),
            token: sid.to_string(),
            token_label: "server SteamID",
            call: gs_call("/IGameServersService/SetMemo/v1/")
                .param("steamid", sid)
                .param("memo", &memo),
            invalidate: gs_invalidate(Some(sid)),
            secret_fields: &[],
            done: format!("Memo updated for {sid}."),
            subject: obj_map(&[
                ("steamid", sid.to_string().into()),
                ("memo", memo.as_str().into()),
            ]),
        })
    })();
    run_write(env, "steam_set_game_server_memo", args, op)
}

fn reset_gs_token(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let sid = steamid(args, "steamid")?;
        Ok(WriteOp {
            action: "game server token reset",
            prompt: format!(
                "Generate a new login token for game server account {sid}? The current token stops working immediately."
            ),
            token: sid.to_string(),
            token_label: "server SteamID",
            call: gs_call("/IGameServersService/ResetLoginToken/v1/").param("steamid", sid),
            invalidate: gs_invalidate(Some(sid)),
            secret_fields: &["login_token"],
            done: format!("Issued a new login token for {sid}."),
            subject: obj_map(&[("steamid", sid.to_string().into())]),
        })
    })();
    run_write(env, "steam_reset_game_server_token", args, op)
}

fn delete_gs_account(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let sid = steamid(args, "steamid")?;
        Ok(WriteOp {
            action: "game server account deleted",
            prompt: format!(
                "Delete game server account {sid}? Servers using its token can no longer log on. This cannot be undone."
            ),
            token: sid.to_string(),
            token_label: "server SteamID",
            call: gs_call("/IGameServersService/DeleteAccount/v1/").param("steamid", sid),
            invalidate: gs_invalidate(Some(sid)),
            secret_fields: &[],
            done: format!("Deleted game server account {sid}."),
            subject: obj_map(&[("steamid", sid.to_string().into())]),
        })
    })();
    run_write(env, "steam_delete_game_server_account", args, op)
}

fn set_gs_ban(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let op = (|| {
        let sid = steamid(args, "steamid")?;
        let banned = arg_bool(args, "banned", true);
        let seconds = arg_u64(args, "ban_seconds").unwrap_or(0);
        let what = if !banned {
            "Lift the ban on".to_string()
        } else if seconds == 0 {
            "Permanently ban".to_string()
        } else {
            format!("Ban for {seconds} seconds")
        };
        Ok(WriteOp {
            action: "game server ban status set",
            prompt: format!(
                "{what} game server account {sid} (applies to every server using its token)?"
            ),
            token: sid.to_string(),
            token_label: "server SteamID",
            call: Call::post(
                Host::Partner,
                "/IGameServersService/SetBanStatus/v1/",
                KeyKind::Publisher,
                "volatile",
            )
            .param("steamid", sid)
            .param("banned", banned)
            .param("ban_seconds", seconds),
            invalidate: gs_invalidate(Some(sid)),
            secret_fields: &[],
            done: format!(
                "Ban status for {sid} set to {}.",
                if banned { "banned" } else { "not banned" }
            ),
            subject: obj_map(&[
                ("steamid", sid.to_string().into()),
                ("banned", banned.into()),
                ("banSeconds", seconds.into()),
            ]),
        })
    })();
    run_write(env, "steam_partner_set_game_server_ban", args, op)
}

pub(super) fn obj_map(pairs: &[(&str, Value)]) -> Map {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(*k, v.clone());
    }
    m
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) fn def(
    name: &'static str,
    title: &'static str,
    desc: &str,
    key: KeyKind,
    destructive: bool,
    idempotent: bool,
    schema: Value,
    handler: Handler,
) -> ToolDef {
    ToolDef {
        name,
        title,
        description: format!(
            "{desc} Requires STEAM_MCP_ENABLE_WRITES and user confirmation via form elicitation."
        ),
        input_schema: schema,
        output_schema: None,
        hints: Hints {
            read_only: false,
            destructive,
            idempotent,
            open_world: true,
        },
        scope: scopes::WRITE,
        key,
        task: false,
        write: true,
        handler,
    }
}

fn board_id() -> Value {
    p_u64("Leaderboard ID from steam_partner_get_leaderboards.")
}

fn board_name() -> Value {
    obj! { "type" => "string", "minLength" => 1, "maxLength" => 128, "description" => "Leaderboard name." }
}

fn memo() -> Value {
    obj! { "type" => "string", "maxLength" => 255, "description" => "Free-text note shown in the account list." }
}

pub fn tools() -> Vec<ToolDef> {
    let pk = KeyKind::Publisher;
    let k = KeyKind::Public;
    vec![
        def(
            "steam_partner_create_leaderboard",
            "Create Leaderboard",
            "Finds or creates a leaderboard (FindOrCreateLeaderboard).",
            pk,
            false,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("name", board_name()),
                    (
                        "sort_method",
                        p_enum(
                            "Ranking order.",
                            &["Ascending", "Descending"],
                            Some("Descending"),
                        ),
                    ),
                    (
                        "display_type",
                        p_enum(
                            "How scores display.",
                            &["Numeric", "TimeSeconds", "TimeMilliSeconds"],
                            Some("Numeric"),
                        ),
                    ),
                    (
                        "only_trusted_writes",
                        p_bool("Only trusted servers can write scores.", false),
                    ),
                    (
                        "only_friends_reads",
                        p_bool("Users can only read friends' scores.", false),
                    ),
                ],
                &["appid", "name"],
            ),
            find_or_create_leaderboard,
        ),
        def(
            "steam_partner_set_leaderboard_score",
            "Set Leaderboard Score",
            "Sets a user's leaderboard score (SetLeaderboardScore).",
            pk,
            false,
            false,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("leaderboardid", board_id()),
                    ("steamid", p_steamid()),
                    (
                        "score",
                        obj! { "type" => "integer", "description" => "Score (32-bit signed)." , "minimum" => -2147483648i64, "maximum" => 2147483647i64 },
                    ),
                    (
                        "score_method",
                        p_enum(
                            "KeepBest keeps the better score; ForceUpdate overwrites.",
                            &["KeepBest", "ForceUpdate"],
                            Some("KeepBest"),
                        ),
                    ),
                    (
                        "details_hex",
                        obj! { "type" => "string", "pattern" => "^([0-9a-fA-F]{2})*$", "maxLength" => 512, "description" => "Optional game-specific details, hex-encoded." },
                    ),
                ],
                &["appid", "leaderboardid", "steamid", "score"],
            ),
            set_leaderboard_score,
        ),
        def(
            "steam_partner_delete_leaderboard_score",
            "Delete Leaderboard Score",
            "Deletes a user's leaderboard entry (DeleteLeaderboardScore).",
            pk,
            true,
            true,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("leaderboardid", board_id()),
                    ("steamid", p_steamid()),
                ],
                &["appid", "leaderboardid", "steamid"],
            ),
            delete_leaderboard_score,
        ),
        def(
            "steam_partner_reset_leaderboard",
            "Reset Leaderboard",
            "Removes every entry from a leaderboard (ResetLeaderboard).",
            pk,
            true,
            true,
            object_schema(
                vec![("appid", p_appid()), ("leaderboardid", board_id())],
                &["appid", "leaderboardid"],
            ),
            reset_leaderboard,
        ),
        def(
            "steam_partner_delete_leaderboard",
            "Delete Leaderboard",
            "Deletes a leaderboard and its entries (DeleteLeaderboard).",
            pk,
            true,
            true,
            object_schema(
                vec![("appid", p_appid()), ("name", board_name())],
                &["appid", "name"],
            ),
            delete_leaderboard,
        ),
        def(
            "steam_create_game_server_account",
            "Create Game Server Account",
            "Creates a persistent game server account and returns its login token (CreateAccount). Never cached or logged.",
            k,
            false,
            false,
            object_schema(
                vec![("appid", p_appid()), ("memo", memo())],
                &["appid", "memo"],
            ),
            create_gs_account,
        ),
        def(
            "steam_set_game_server_memo",
            "Set Game Server Memo",
            "Changes a game server account's memo (SetMemo).",
            k,
            false,
            true,
            object_schema(
                vec![("steamid", p_steamid()), ("memo", memo())],
                &["steamid", "memo"],
            ),
            set_gs_memo,
        ),
        def(
            "steam_reset_game_server_token",
            "Reset Game Server Token",
            "Issues a new login token for a game server account, revoking the old one (ResetLoginToken).",
            k,
            true,
            false,
            object_schema(vec![("steamid", p_steamid())], &["steamid"]),
            reset_gs_token,
        ),
        def(
            "steam_delete_game_server_account",
            "Delete Game Server Account",
            "Deletes a game server account (DeleteAccount).",
            k,
            true,
            true,
            object_schema(vec![("steamid", p_steamid())], &["steamid"]),
            delete_gs_account,
        ),
        def(
            "steam_partner_set_game_server_ban",
            "Ban Game Server Account",
            "Bans or unbans a game server account for your app (SetBanStatus).",
            pk,
            true,
            true,
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    (
                        "banned",
                        p_bool("Ban (true) or lift the ban (false).", true),
                    ),
                    (
                        "ban_seconds",
                        p_int(
                            "Ban length in seconds (0 = permanent).",
                            0,
                            315_360_000,
                            Some(0),
                        ),
                    ),
                ],
                &["steamid"],
            ),
            set_gs_ban,
        ),
    ]
}
