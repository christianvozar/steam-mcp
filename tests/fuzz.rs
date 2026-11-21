//! Deterministic fuzzing of every hand-written parser and the dispatcher.
//!
//! A seeded xorshift PRNG generates random and mutated inputs. Properties:
//! no panics, bounded work, and exact JSON round-trips. Override the seed
//! with `FUZZ_SEED` and the iteration count with `FUZZ_ITERS` to explore.

mod common;

use std::io::{self, BufReader, Read, Write};
use std::sync::Arc;

use common::*;
use steam_mcp::json::{self, Value};
use steam_mcp::mcp::context::{NullSink, Principal, Transport};
use steam_mcp::mcp::server::Reply;
use steam_mcp::obj;
use steam_mcp::util::CancelToken;

struct Rng(u64);

impl Rng {
    fn new(salt: u64) -> Rng {
        let seed = std::env::var("FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x5EED_CAFE_u64);
        Rng((seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn iters(default: usize) -> usize {
    std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Mutates a seed input: flips, inserts, deletes, duplicates or splices bytes.
fn mutate(rng: &mut Rng, input: &[u8], dictionary: &[&[u8]]) -> Vec<u8> {
    let mut v = input.to_vec();
    for _ in 0..1 + rng.below(4) {
        let pos = if v.is_empty() { 0 } else { rng.below(v.len()) };
        match rng.below(6) {
            0 if !v.is_empty() => v[pos] ^= 1 << rng.below(8),
            1 => v.insert(pos, rng.next() as u8),
            2 if !v.is_empty() => {
                v.remove(pos);
            }
            3 => {
                let tok = rng.pick(dictionary);
                v.splice(pos..pos, tok.iter().cloned());
            }
            4 if !v.is_empty() => {
                let end = (pos + rng.below(16)).min(v.len());
                let chunk: Vec<u8> = v[pos..end].to_vec();
                v.splice(pos..pos, chunk);
            }
            _ => v.truncate(pos),
        }
    }
    v
}

/// Random JSON value generator (bounded depth).
fn random_value(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth > 3 { 5 } else { 7 }) {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => match rng.below(3) {
            0 => Value::from(rng.next() as i64),
            1 => Value::from(rng.next()),
            _ => Value::from((rng.next() as f64) / 1e9 - 9e9),
        },
        3 => {
            let pool = [
                "",
                "570",
                "abc",
                "76561197960287930",
                "\u{0}\u{1f}",
                "é😀",
                "\"\\/",
                "true",
                "2026-07-28",
                "x".repeat(50).leak(),
            ];
            Value::from(*rng.pick(&pool))
        }
        4 => Value::from(String::from_utf8_lossy(&rng.bytes(12)).into_owned()),
        5 => Value::Array(
            (0..rng.below(5))
                .map(|_| random_value(rng, depth + 1))
                .collect(),
        ),
        _ => {
            let keys = [
                "appid",
                "steamid",
                "steamids",
                "query",
                "limit",
                "name",
                "arguments",
                "_meta",
                "cursor",
                "uri",
                "x",
            ];
            let mut m = json::Map::new();
            for _ in 0..rng.below(5) {
                m.insert(*rng.pick(&keys), random_value(rng, depth + 1));
            }
            Value::Object(m)
        }
    }
}

#[test]
fn json_parser_never_panics_and_round_trips() {
    let mut rng = Rng::new(1);
    let seeds: [&[u8]; 6] = [
        b"{\"a\":[1,2.5,-3e10,true,null,\"x\\u00e9\\ud83d\\ude00\"],\"b\":{\"c\":18446744073709551615}}",
        b"[[[[[]]]]]",
        br#""\n\t\"\\""#,
        b"-0.0e-0",
        br#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#,
        b"   7   ",
    ];
    let dict: [&[u8]; 10] = [
        b"{", b"}", b"[", b"]", b"\"", b"\\u", b"\\ud83d", b",", b":", b"1e999",
    ];
    for _ in 0..iters(20_000) {
        let input = if rng.below(4) == 0 {
            rng.bytes(64)
        } else {
            {
                let seed = rng_pick_bytes(&mut rng, &seeds);
                mutate(&mut rng, seed, &dict)
            }
        };
        if let Ok(v) = json::parse_bytes(&input) {
            let compact = v.to_string();
            assert_eq!(
                json::parse(&compact).as_ref(),
                Ok(&v),
                "compact round trip of {compact}"
            );
            assert_eq!(json::parse(&v.to_pretty_string()).as_ref(), Ok(&v));
        }
    }
    // Generated values must always serialize to parseable JSON.
    for _ in 0..iters(5_000) {
        let v = random_value(&mut rng, 0);
        assert_eq!(json::parse(&v.to_string()).unwrap(), v);
    }
}

fn rng_pick_bytes<'a>(rng: &mut Rng, seeds: &'a [&'a [u8]]) -> &'a [u8] {
    seeds[rng.below(seeds.len())]
}

struct Duplex(io::Cursor<Vec<u8>>);
impl Read for Duplex {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for Duplex {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn http_parsers_never_panic() {
    let mut rng = Rng::new(2);
    let seeds: [&[u8]; 4] = [
        b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        b"POST /mcp?x=1 HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n2\r\n{}\r\n0\r\nT: v\r\n\r\n",
        b"GET http://h/p HTTP/1.0\r\n\r\n",
        b"\r\n\r\nPOST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
    ];
    let dict: [&[u8]; 9] = [
        b"\r\n",
        b": ",
        b"Content-Length: 99999999999",
        b"Transfer-Encoding: chunked",
        b"ffffffff\r\n",
        b"\t",
        b" ",
        b"\0",
        b"HTTP/1.1",
    ];
    for _ in 0..iters(20_000) {
        let input = if rng.below(5) == 0 {
            rng.bytes(128)
        } else {
            {
                let seed = rng_pick_bytes(&mut rng, &seeds);
                mutate(&mut rng, seed, &dict)
            }
        };
        let mut r = BufReader::new(Duplex(io::Cursor::new(input.clone())));
        let _ = steam_mcp::http::server::read_request(&mut r, 4096, 4096);
        let mut r = BufReader::new(&input[..]);
        let _ = steam_mcp::http::read_body(&mut r, &steam_mcp::http::Framing::Chunked, 4096);
        let text = String::from_utf8_lossy(&input);
        let _ = steam_mcp::http::Url::parse(&text);
        let _ = steam_mcp::transport::http::decode_header_value(&text);
    }
}

#[test]
fn encoders_templates_and_state_never_panic() {
    let mut rng = Rng::new(3);
    let sealer = steam_mcp::mcp::state::StateSealer::new(b"k".to_vec());
    let salient = obj! { "name" => "t" };
    let binding = steam_mcp::mcp::state::Binding {
        method: "tools/call",
        salient: &salient,
        principal: "p",
    };
    let good = sealer.seal(
        &binding,
        obj! { "step" => 1 },
        std::time::Duration::from_secs(60),
    );
    let dict: [&[u8]; 6] = [b".", b"v1.", b"=", b"{", b"%", b"}"];
    for _ in 0..iters(20_000) {
        let raw = mutate(&mut rng, good.as_bytes(), &dict);
        let s = String::from_utf8_lossy(&raw);
        if s != good {
            assert!(
                sealer.open(&s, &binding).is_err(),
                "mutated state accepted: {s}"
            );
        }
        let _ = steam_mcp::util::encoding::b64_decode(&s);
        let _ = steam_mcp::util::encoding::pct_decode(&s, rng.below(2) == 0);
        let _ = steam_mcp::util::time::parse_datetime(&s);
        let tpl_src =
            String::from_utf8_lossy(&mutate(&mut rng, b"steam://app/{appid}/x{?a,b}{+p}", &dict))
                .into_owned();
        if let Some(t) = steam_mcp::util::uritemplate::UriTemplate::parse(&tpl_src) {
            let vars = [("appid".to_string(), s.to_string())].into_iter().collect();
            let expanded = t.expand(&vars);
            let _ = t.matches(&expanded);
            let _ = t.matches(&s);
        }
    }
}

#[test]
fn regex_engine_is_panic_free_and_bounded() {
    let mut rng = Rng::new(4);
    let atoms = [
        "a", "b", ".", "\\d", "\\w", "\\S", "[a-c]", "[^x]", "(", ")", "|", "*", "+", "?", "{2}",
        "{1,3}", "^", "$", "(?:", "\\", "[", "]", "{", "-", "é",
    ];
    for _ in 0..iters(10_000) {
        let pat: String = (0..1 + rng.below(10)).map(|_| *rng.pick(&atoms)).collect();
        if let Ok(re) = steam_mcp::util::regex::Regex::new(&pat) {
            let subject: String = (0..rng.below(30))
                .map(|_| *rng.pick(&["a", "b", "c", "1", "x", " ", "é"]))
                .collect();
            let started = std::time::Instant::now();
            let _ = re.is_match(&subject);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1),
                "pattern {pat} took too long"
            );
        }
    }
}

#[test]
fn schema_validation_handles_arbitrary_arguments() {
    let mut rng = Rng::new(5);
    let tools = steam_mcp::mcp::tools::all_tools();
    for _ in 0..iters(10_000) {
        let t = &tools[rng.below(tools.len())];
        let mut v = random_value(&mut rng, 0);
        steam_mcp::mcp::schema::coerce(&t.input_schema, &mut v);
        let _ = steam_mcp::mcp::schema::validate(&t.input_schema, &v);
    }
}

#[test]
fn dispatcher_survives_arbitrary_messages() {
    // Upstream is unreachable, so handlers fail fast after validation.
    let mock = start_mock();
    let core = core_with(
        &mock,
        &[
            ("STEAM_MCP_API_BASE", "http://127.0.0.1:9"),
            ("STEAM_MCP_STORE_BASE", "http://127.0.0.1:9"),
            ("STEAM_MCP_PARTNER_BASE", "http://127.0.0.1:9"),
        ],
    );
    let mut rng = Rng::new(6);
    let methods = [
        "server/discover",
        "tools/list",
        "tools/call",
        "resources/list",
        "resources/read",
        "resources/templates/list",
        "prompts/list",
        "prompts/get",
        "completion/complete",
        "tasks/get",
        "tasks/update",
        "tasks/cancel",
        "initialize",
        "bogus",
        "notifications/cancelled",
    ];
    let tools: Vec<&'static str> = steam_mcp::mcp::tools::all_tools()
        .iter()
        .map(|t| t.name)
        .collect();
    for i in 0..iters(3_000) {
        let method = *rng.pick(&methods);
        let mut params = match random_value(&mut rng, 0) {
            v @ Value::Object(_) => v,
            _ => obj! {},
        };
        match method {
            "tools/call" => {
                params.set("name", *rng.pick(&tools));
                params.set("arguments", random_value(&mut rng, 1));
            }
            "resources/read" => params.set(
                "uri",
                *rng.pick(&[
                    "steam://app/570/details",
                    "steam://nope",
                    "steam://player/x/games",
                    "steam://partner/sales/2026",
                ]),
            ),
            "prompts/get" => params.set(
                "name",
                *rng.pick(&["game_health_report", "nope", "partner_sales_report"]),
            ),
            _ => {}
        }
        let mut msg = if rng.below(10) == 0 {
            random_value(&mut rng, 0)
        } else {
            request(
                i as i64,
                method,
                params,
                if rng.below(3) == 0 {
                    random_value(&mut rng, 2)
                } else {
                    obj! {}
                },
            )
        };
        if rng.below(8) == 0 {
            if let Some(m) = msg.as_object_mut() {
                m.remove("id");
            }
        }
        match core.handle_message(
            &msg,
            Principal::local(),
            Transport::Internal,
            Arc::new(NullSink),
            CancelToken::new(),
        ) {
            Reply::Response(r, status) => {
                assert_eq!(r["jsonrpc"].as_str(), Some("2.0"));
                assert!(r.get("result").is_some() != r.get("error").is_some(), "{r}");
                assert_ne!(
                    r["error"]["message"].as_str(),
                    Some("internal server error"),
                    "handler panicked on {msg}"
                );
                assert!((200..600).contains(&status));
                // Must round-trip as a single line (stdio framing).
                assert!(!r.to_string().contains('\n'));
            }
            Reply::NoResponse => {}
            Reply::Subscription(s) => s.close(),
        }
    }
}
