//! OAuth 2.1 resource-server support for the Streamable HTTP transport.
//!
//! * Protected Resource Metadata (RFC 9728) and `WWW-Authenticate` challenges.
//! * Bearer validation via static tokens, JWTs (HS256 with an in-tree HMAC;
//!   RS256/ES256 through `ring` with a cached JWKS), or token introspection
//!   (RFC 7662, cached).
//! * Audience binding (RFC 8707): tokens must name this server.
//! * Tokens are never forwarded upstream; Steam calls use server-held keys.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{AuthMode, Config};
use crate::http::client::{HttpClient, Request};
use crate::json::{self, Value};
use crate::mcp::context::Principal;
use crate::obj;
use crate::util::crypto::{ct_eq, hmac_sha256, sha256, sha256_hex};
use crate::util::encoding::{b64_decode, b64_encode, build_query};
use crate::util::time::now_secs;

/// An authentication failure with the challenge to return.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthFailure {
    pub status: u16,
    pub error: Option<&'static str>,
    pub description: String,
}

impl AuthFailure {
    fn missing() -> AuthFailure {
        AuthFailure { status: 401, error: None, description: "Authorization required".into() }
    }
    fn invalid(desc: impl Into<String>) -> AuthFailure {
        AuthFailure { status: 401, error: Some("invalid_token"), description: desc.into() }
    }
    fn bad_request(desc: impl Into<String>) -> AuthFailure {
        AuthFailure { status: 400, error: Some("invalid_request"), description: desc.into() }
    }
}

struct Jwks {
    fetched: Instant,
    keys: Vec<Value>,
}

pub struct Authenticator {
    http: Arc<HttpClient>,
    jwks: Mutex<Option<Jwks>>,
    last_jwks_refresh: Mutex<Option<Instant>>,
    introspection_cache: Mutex<HashMap<String, (Instant, Option<Principal>)>>,
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Authenticator {
    pub fn new(cfg: &Config) -> Result<Authenticator, String> {
        let http = HttpClient::new(crate::http::client::ClientOptions { extra_ca_file: cfg.ca_file.clone(), use_env_proxy: cfg.use_proxy_env, io_timeout: Duration::from_secs(10), ..Default::default() }).map_err(|e| e.to_string())?;
        Ok(Authenticator { http: Arc::new(http), jwks: Mutex::new(None), last_jwks_refresh: Mutex::new(None), introspection_cache: Mutex::new(HashMap::new()) })
    }

    /// The canonical resource identifier for this server.
    pub fn resource_url(cfg: &Config, host_header: Option<&str>, tls: bool) -> String {
        match &cfg.public_url {
            Some(u) => u.clone(),
            None => format!("{}://{}{}", if tls { "https" } else { "http" }, host_header.unwrap_or("localhost"), cfg.http_path),
        }
    }

    /// RFC 9728 metadata URL (path-inserted form).
    pub fn metadata_url(cfg: &Config, host_header: Option<&str>, tls: bool) -> String {
        let resource = Self::resource_url(cfg, host_header, tls);
        match crate::http::Url::parse(&resource) {
            Some(u) => {
                let path = u.path().trim_end_matches('/');
                format!("{}/.well-known/oauth-protected-resource{}", u.origin(), if path == "/" { "" } else { path })
            }
            None => format!("{resource}/.well-known/oauth-protected-resource"),
        }
    }

    /// The Protected Resource Metadata document.
    pub fn metadata(cfg: &Config, host_header: Option<&str>, tls: bool, scopes: &[&str]) -> Value {
        let mut doc = obj! {
            "resource" => Self::resource_url(cfg, host_header, tls),
            "authorization_servers" => cfg.auth.authorization_servers.clone(),
            "scopes_supported" => scopes.to_vec(),
            "bearer_methods_supported" => crate::arr!["header"],
            "resource_name" => "Steam MCP Server",
            "resource_documentation" => "https://github.com/christianvozar/steam-mcp#authorization",
        };
        if cfg.auth.mode == AuthMode::Jwt && cfg.auth.jwks_url.is_some() {
            doc.set("resource_signing_alg_values_supported", crate::arr!["RS256", "ES256", "HS256"]);
        }
        doc
    }

    /// `WWW-Authenticate` value for a failure or scope challenge.
    pub fn challenge(metadata_url: &str, error: Option<&str>, description: Option<&str>, scope: Option<&str>) -> String {
        let mut parts = vec![format!("resource_metadata=\"{}\"", quote(metadata_url))];
        if let Some(e) = error {
            parts.insert(0, format!("error=\"{e}\""));
        }
        if let Some(s) = scope {
            parts.push(format!("scope=\"{}\"", quote(s)));
        }
        if let Some(d) = description {
            parts.push(format!("error_description=\"{}\"", quote(&d.replace(['\r', '\n'], " "))));
        }
        format!("Bearer {}", parts.join(", "))
    }

    /// Authenticates a request from its `Authorization` header.
    pub fn authenticate(&self, cfg: &Config, authorization: Option<&str>) -> Result<Principal, AuthFailure> {
        if cfg.auth.mode == AuthMode::None {
            return Ok(Principal::local());
        }
        let Some(header) = authorization else { return Err(AuthFailure::missing()) };
        let (scheme, token) = header.trim().split_once(' ').ok_or_else(|| AuthFailure::bad_request("Malformed Authorization header"))?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(AuthFailure::bad_request("Only the Bearer scheme is supported"));
        }
        let token = token.trim();
        if token.is_empty() || token.len() > 16 * 1024 {
            return Err(AuthFailure::invalid("Malformed bearer token"));
        }
        match cfg.auth.mode {
            AuthMode::None => Ok(Principal::local()),
            AuthMode::Static => {
                let h = sha256(token.as_bytes());
                cfg.auth
                    .static_tokens
                    .iter()
                    .find(|t| ct_eq(&t.token_hash, &h))
                    .map(|t| Principal { id: t.principal.clone(), scopes: t.scopes.clone(), authenticated: true })
                    .ok_or_else(|| AuthFailure::invalid("Unknown token"))
            }
            AuthMode::Jwt => self.validate_jwt(cfg, token),
            AuthMode::Introspection => self.introspect(cfg, token),
        }
    }

    fn check_claims(cfg: &Config, claims: &Value) -> Result<Principal, AuthFailure> {
        let now = now_secs();
        let skew = cfg.auth.clock_skew_secs;
        let exp = claims["exp"].as_u64().ok_or_else(|| AuthFailure::invalid("Token has no expiry"))?;
        if now > exp + skew {
            return Err(AuthFailure::invalid("Token expired"));
        }
        if let Some(nbf) = claims["nbf"].as_u64() {
            if nbf > now + skew {
                return Err(AuthFailure::invalid("Token not yet valid"));
            }
        }
        if let Some(iss) = &cfg.auth.issuer {
            if claims["iss"].as_str() != Some(iss.as_str()) {
                return Err(AuthFailure::invalid("Token issuer is not trusted"));
            }
        }
        let auds: Vec<String> = match &claims["aud"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => Vec::new(),
        };
        let normalize = |s: &str| s.trim_end_matches('/').to_string();
        if !auds.iter().any(|a| cfg.auth.audiences.iter().any(|ours| normalize(a) == normalize(ours))) {
            return Err(AuthFailure::invalid("Token was not issued for this resource (audience mismatch)"));
        }
        let scopes: Vec<String> = match (&claims["scope"], &claims["scp"]) {
            (Value::String(s), _) => s.split_whitespace().map(String::from).collect(),
            (_, Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            (_, Value::String(s)) => s.split_whitespace().map(String::from).collect(),
            _ => Vec::new(),
        };
        let id = claims["sub"].as_str().or_else(|| claims["client_id"].as_str()).unwrap_or("unknown").to_string();
        Ok(Principal { id, scopes, authenticated: true })
    }

    fn validate_jwt(&self, cfg: &Config, token: &str) -> Result<Principal, AuthFailure> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(AuthFailure::invalid("Token is not a JWT"));
        }
        let header = b64_decode(parts[0]).and_then(|b| json::parse_bytes(&b).ok()).ok_or_else(|| AuthFailure::invalid("Malformed JWT header"))?;
        let claims = b64_decode(parts[1]).and_then(|b| json::parse_bytes(&b).ok()).ok_or_else(|| AuthFailure::invalid("Malformed JWT claims"))?;
        let sig = b64_decode(parts[2]).ok_or_else(|| AuthFailure::invalid("Malformed JWT signature"))?;
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let alg = header["alg"].as_str().unwrap_or("");
        let verified = match alg {
            "HS256" => {
                let secret = cfg.auth.jwt_hs256_secret.as_ref().ok_or_else(|| AuthFailure::invalid("HS256 tokens are not accepted"))?;
                ct_eq(&hmac_sha256(secret.expose().as_bytes(), signing_input.as_bytes()), &sig)
            }
            "RS256" | "ES256" => {
                let kid = header["kid"].as_str();
                let key = self.find_jwk(cfg, kid, alg)?;
                verify_asymmetric(alg, &key, signing_input.as_bytes(), &sig)
            }
            _ => return Err(AuthFailure::invalid(format!("Unsupported JWT algorithm '{alg}'"))),
        };
        if !verified {
            return Err(AuthFailure::invalid("Invalid token signature"));
        }
        if header["typ"].as_str().is_some_and(|t| !(t.eq_ignore_ascii_case("JWT") || t.eq_ignore_ascii_case("at+jwt") || t.eq_ignore_ascii_case("application/at+jwt"))) {
            return Err(AuthFailure::invalid("Token is not an access token"));
        }
        Self::check_claims(cfg, &claims)
    }

    fn fetch_jwks(&self, url: &str) -> Result<Vec<Value>, AuthFailure> {
        let resp = self.http.send(&Request::get(url)).map_err(|e| AuthFailure::invalid(format!("Could not fetch JWKS: {e}")))?;
        if resp.status != 200 {
            return Err(AuthFailure::invalid(format!("JWKS endpoint returned {}", resp.status)));
        }
        let v = json::parse_bytes(&resp.body).map_err(|_| AuthFailure::invalid("JWKS is not valid JSON"))?;
        Ok(v["keys"].as_array().cloned().unwrap_or_default())
    }

    fn find_jwk(&self, cfg: &Config, kid: Option<&str>, alg: &str) -> Result<Value, AuthFailure> {
        let url = cfg.auth.jwks_url.as_deref().ok_or_else(|| AuthFailure::invalid("Asymmetric tokens are not accepted"))?;
        let kty = if alg == "RS256" { "RSA" } else { "EC" };
        let pick = |keys: &[Value]| keys.iter().find(|k| k["kty"].as_str() == Some(kty) && kid.is_none_or(|kid| k["kid"].as_str() == Some(kid)) && k["use"].as_str().is_none_or(|u| u == "sig")).cloned();
        {
            let cache = self.jwks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(j) = cache.as_ref().filter(|j| j.fetched.elapsed() < Duration::from_secs(3600)) {
                if let Some(k) = pick(&j.keys) {
                    return Ok(k);
                }
            }
        }
        // Unknown kid or stale cache: refresh at most once a minute.
        {
            let mut last = self.last_jwks_refresh.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) && self.jwks.lock().map(|j| j.is_some()).unwrap_or(false) {
                return Err(AuthFailure::invalid("Signing key not found"));
            }
            *last = Some(Instant::now());
        }
        let keys = self.fetch_jwks(url)?;
        let found = pick(&keys);
        *self.jwks.lock().unwrap_or_else(|e| e.into_inner()) = Some(Jwks { fetched: Instant::now(), keys });
        found.ok_or_else(|| AuthFailure::invalid("Signing key not found"))
    }

    fn introspect(&self, cfg: &Config, token: &str) -> Result<Principal, AuthFailure> {
        let key = sha256_hex(token.as_bytes());
        {
            let mut cache = self.introspection_cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() > 10_000 {
                cache.retain(|_, (exp, _)| *exp > Instant::now());
            }
            if let Some((until, p)) = cache.get(&key) {
                if *until > Instant::now() {
                    return p.clone().ok_or_else(|| AuthFailure::invalid("Token is not active"));
                }
            }
        }
        let url = cfg.auth.introspection_url.as_deref().ok_or_else(|| AuthFailure::invalid("Introspection is not configured"))?;
        let mut req = Request::post_form(url, build_query([("token", token), ("token_type_hint", "access_token")]));
        req.idempotent = true;
        if let (Some(id), Some(secret)) = (&cfg.auth.introspection_client_id, &cfg.auth.introspection_client_secret) {
            let creds = format!("{}:{}", crate::util::encoding::pct_encode(id), crate::util::encoding::pct_encode(secret.expose()));
            req = req.header("Authorization", &format!("Basic {}", b64_encode(creds.as_bytes())));
        }
        let resp = self.http.send(&req).map_err(|e| AuthFailure::invalid(format!("Introspection failed: {e}")))?;
        if resp.status != 200 {
            return Err(AuthFailure::invalid(format!("Introspection endpoint returned {}", resp.status)));
        }
        let v = json::parse_bytes(&resp.body).map_err(|_| AuthFailure::invalid("Introspection response is not JSON"))?;
        let result = if v["active"].as_bool() == Some(true) {
            // Introspection may omit exp; treat as short-lived.
            let mut claims = v.clone();
            if claims["exp"].is_null() {
                claims.set("exp", now_secs() + 60);
            }
            Self::check_claims(cfg, &claims).ok()
        } else {
            None
        };
        let ttl = match &result {
            Some(_) => Duration::from_secs(v["exp"].as_u64().map(|e| e.saturating_sub(now_secs())).unwrap_or(60).min(60)),
            None => Duration::from_secs(10),
        };
        self.introspection_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now() + ttl, result.clone()));
        result.ok_or_else(|| AuthFailure::invalid("Token is not active or not valid for this resource"))
    }
}

fn verify_asymmetric(alg: &str, jwk: &Value, msg: &[u8], sig: &[u8]) -> bool {
    use ring::signature;
    match alg {
        "RS256" => {
            let (Some(n), Some(e)) = (jwk["n"].as_str().and_then(b64_decode), jwk["e"].as_str().and_then(b64_decode)) else { return false };
            signature::RsaPublicKeyComponents { n: &n, e: &e }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig).is_ok()
        }
        "ES256" => {
            if jwk["crv"].as_str() != Some("P-256") {
                return false;
            }
            let (Some(x), Some(y)) = (jwk["x"].as_str().and_then(b64_decode), jwk["y"].as_str().and_then(b64_decode)) else { return false };
            if x.len() != 32 || y.len() != 32 {
                return false;
            }
            let mut point = vec![0x04];
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &point).verify(msg, sig).is_ok()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::encoding::b64url_encode;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        Config::from_map(&pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap()
    }

    fn hs256(secret: &str, claims: &Value) -> String {
        let h = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let c = b64url_encode(claims.to_string().as_bytes());
        let sig = hmac_sha256(secret.as_bytes(), format!("{h}.{c}").as_bytes());
        format!("{h}.{c}.{}", b64url_encode(&sig))
    }

    #[test]
    fn jwt_hs256() {
        let c = cfg(&[("STEAM_MCP_AUTH_MODE", "jwt"), ("STEAM_MCP_JWT_HS256_SECRET", "topsecret"), ("STEAM_MCP_PUBLIC_URL", "https://mcp.example.com/mcp"), ("STEAM_MCP_AUTH_ISSUER", "https://auth.example.com"), ("STEAM_MCP_AUTHORIZATION_SERVERS", "https://auth.example.com")]);
        let a = Authenticator::new(&c).unwrap();
        let good = obj! { "sub" => "alice", "aud" => "https://mcp.example.com/mcp", "iss" => "https://auth.example.com", "exp" => now_secs() + 300, "scope" => "steam:public steam:partner" };
        let p = a.authenticate(&c, Some(&format!("Bearer {}", hs256("topsecret", &good)))).unwrap();
        assert_eq!(p.id, "alice");
        assert!(p.has_scope("steam:partner"));
        assert!(!p.has_scope("steam:financial"));

        let wrong_aud = good.clone().with("aud", "https://other.example.com");
        assert_eq!(a.authenticate(&c, Some(&format!("Bearer {}", hs256("topsecret", &wrong_aud)))).unwrap_err().error, Some("invalid_token"));
        let expired = good.clone().with("exp", now_secs() - 3600);
        assert!(a.authenticate(&c, Some(&format!("Bearer {}", hs256("topsecret", &expired)))).is_err());
        let wrong_iss = good.clone().with("iss", "https://evil.example.com");
        assert!(a.authenticate(&c, Some(&format!("Bearer {}", hs256("topsecret", &wrong_iss)))).is_err());
        assert!(a.authenticate(&c, Some(&format!("Bearer {}", hs256("wrong", &good)))).is_err());
        let none_alg = format!("{}.{}.", b64url_encode(br#"{"alg":"none"}"#), b64url_encode(good.to_string().as_bytes()));
        assert!(a.authenticate(&c, Some(&format!("Bearer {none_alg}"))).is_err());
        assert_eq!(a.authenticate(&c, None).unwrap_err().status, 401);
        assert_eq!(a.authenticate(&c, Some("Basic abc")).unwrap_err().status, 400);
    }

    #[test]
    fn static_tokens_and_metadata() {
        let c = cfg(&[("STEAM_MCP_AUTH_MODE", "static"), ("STEAM_MCP_AUTH_TOKENS", "0123456789abcdef-token=ops:steam:admin"), ("STEAM_MCP_PUBLIC_URL", "https://mcp.example.com/mcp")]);
        let a = Authenticator::new(&c).unwrap();
        let p = a.authenticate(&c, Some("Bearer 0123456789abcdef-token")).unwrap();
        assert_eq!(p.id, "ops");
        assert!(p.has_scope("steam:financial"));
        assert!(a.authenticate(&c, Some("Bearer nope-nope-nope-nope")).is_err());
        assert_eq!(Authenticator::metadata_url(&c, None, true), "https://mcp.example.com/.well-known/oauth-protected-resource/mcp");
        let md = Authenticator::metadata(&c, None, true, &["steam:public"]);
        assert_eq!(md["resource"].as_str(), Some("https://mcp.example.com/mcp"));
        let ch = Authenticator::challenge("https://x/.well-known/oauth-protected-resource", Some("insufficient_scope"), Some("need more"), Some("steam:partner"));
        assert_eq!(ch, "Bearer error=\"insufficient_scope\", resource_metadata=\"https://x/.well-known/oauth-protected-resource\", scope=\"steam:partner\", error_description=\"need more\"");
    }

    #[test]
    fn es256_with_ring() {
        use ring::rand::SystemRandom;
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let kp = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let pubkey = kp.public_key().as_ref();
        let jwk = obj! { "kty" => "EC", "crv" => "P-256", "x" => b64url_encode(&pubkey[1..33]), "y" => b64url_encode(&pubkey[33..65]) };
        let sig = kp.sign(&rng, b"hello").unwrap();
        assert!(verify_asymmetric("ES256", &jwk, b"hello", sig.as_ref()));
        assert!(!verify_asymmetric("ES256", &jwk, b"hellp", sig.as_ref()));
    }
}
