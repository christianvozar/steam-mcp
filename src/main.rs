//! `steam-mcp` command-line entry point.

use std::sync::Arc;

use steam_mcp::config::{Config, ConfigHandle, KEYS, Sources, TTL_CLASSES};
use steam_mcp::mcp::server::Core;
use steam_mcp::transport;
use steam_mcp::util::log;

fn usage() -> String {
    let mut s = format!(
        "steam-mcp {} — Model Context Protocol (2026-07-28) server for Steam\n\n\
USAGE:\n    steam-mcp [OPTIONS]\n\n\
OPTIONS:\n\
    --stdio                 Serve MCP over stdin/stdout (default when --http is not given)\n\
    --http <ADDR>           Serve Streamable HTTP on ADDR (e.g. 127.0.0.1:8080)\n\
    --config <FILE>         Load KEY=VALUE settings from FILE (watched for changes)\n\
    --set <KEY=VALUE>       Override a setting (repeatable)\n\
    --log-level <LEVEL>     error | warn | info | debug | trace\n\
    --print-config          Print the effective configuration (secrets redacted) and exit\n\
    --list-tools            Print the tool catalogue and exit\n\
    -h, --help              Show this help\n\
    -V, --version           Show the version\n\n\
SETTINGS (environment variables, config file keys, or --set):\n",
        env!("CARGO_PKG_VERSION")
    );
    for (k, d) in KEYS {
        s.push_str(&format!("    {k:<40} {d}\n"));
    }
    s.push_str("\nTTL CLASSES (default ttl/stale seconds):\n");
    for (c, t, st, d) in TTL_CLASSES {
        s.push_str(&format!("    {c:<14} {t:>6}/{st:<7} {d}\n"));
    }
    s
}

fn main() {
    let mut sources = Sources::default();
    let mut log_override = None;
    let mut print_config = false;
    let mut list_tools = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| fail(&format!("{name} requires a value")));
        match a.as_str() {
            "--stdio" => sources.stdio = true,
            "--http" => sources.http_bind = Some(value("--http")),
            "--config" => sources.file = Some(value("--config").into()),
            "--set" => {
                let kv = value("--set");
                let (k, v) = kv.split_once('=').unwrap_or_else(|| fail("--set expects KEY=VALUE"));
                sources.cli.push((k.trim().to_string(), v.to_string()));
            }
            "--log-level" => log_override = Some(value("--log-level")),
            "--print-config" => print_config = true,
            "--list-tools" => list_tools = true,
            "-h" | "--help" => {
                print!("{}", usage());
                return;
            }
            "-V" | "--version" => {
                println!("steam-mcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => fail(&format!("unknown argument {other:?}; see --help")),
        }
    }
    let mut cfg = Config::load(&sources).unwrap_or_else(|e| fail(&e));
    if let Some(l) = log_override {
        cfg.log_level = l;
    }
    log::set_json(cfg.log_json);
    log::set_level(log::Level::parse(&cfg.log_level).unwrap_or_else(|| fail(&format!("invalid log level {:?}", cfg.log_level))));
    if cfg.http_bind.is_none() {
        cfg.stdio = true;
    }

    if print_config {
        println!("{cfg:#?}");
        return;
    }
    if list_tools {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        for t in steam_mcp::mcp::tools::all_tools() {
            if writeln!(out, "{:<48} {:<16} {}", t.name, t.scope, t.title).is_err() {
                break;
            }
        }
        return;
    }

    let config = ConfigHandle::new(cfg.clone(), sources);
    let core = Core::new(config).unwrap_or_else(|e| fail(&e));
    core.start_background();
    steam_mcp::log_info!(
        "main",
        "steam-mcp starting",
        version = env!("CARGO_PKG_VERSION"),
        public_key = cfg.public_key().is_some(),
        publisher_key = cfg.has_partner(),
        financial_key = cfg.has_financial(),
        cache_file = cfg.cache_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()),
    );

    let http_handle = cfg.http_bind.clone().map(|bind| {
        let server = Arc::new(transport::http::HttpServer::new(core.clone()).unwrap_or_else(|e| fail(&e)));
        std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                if let Err(e) = server.serve(&bind, |_| {}) {
                    fail(&format!("HTTP server error on {bind}: {e}"));
                }
            })
            .unwrap_or_else(|e| fail(&format!("could not start HTTP server: {e}")))
    });

    if cfg.stdio {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        if let Err(e) = transport::stdio::serve(core.clone(), stdin.lock(), stdout) {
            fail(&format!("stdio transport error: {e}"));
        }
        // Stdio EOF ends the process even if HTTP is also running.
        core.shutdown();
        return;
    }
    if let Some(h) = http_handle {
        let _ = h.join();
    }
    core.shutdown();
}

fn fail(msg: &str) -> ! {
    eprintln!("steam-mcp: {msg}");
    std::process::exit(2);
}
