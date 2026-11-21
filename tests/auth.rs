//! End-to-end authorization tests: RS256 JWTs validated against a JWKS fetched
//! over HTTP, and RFC 7662 token introspection with client credentials.

mod common;

use std::collections::HashMap;

use common::*;
use ring::rand::SystemRandom;
use ring::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair};
use steam_mcp::config::Config;
use steam_mcp::json::Value;
use steam_mcp::obj;
use steam_mcp::transport::auth::Authenticator;
use steam_mcp::util::encoding::{b64_decode, b64url_encode};
use steam_mcp::util::time::now_secs;

const RESOURCE: &str = "https://mcp.example.com/mcp";

fn test_key() -> RsaKeyPair {
    let text = include_str!("fixtures/rsa-test-key.pk8.b64");
    let b64: String = text.lines().filter(|l| !l.starts_with('#')).collect();
    RsaKeyPair::from_pkcs8(&b64_decode(&b64).unwrap()).unwrap()
}

/// Minimal DER reader for RSAPublicKey ::= SEQUENCE { n INTEGER, e INTEGER }.
fn rsa_components(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
    fn tlv(d: &[u8]) -> (u8, &[u8], &[u8]) {
        let tag = d[0];
        let (len, hdr) = if d[1] < 0x80 {
            (d[1] as usize, 2)
        } else {
            let n = (d[1] & 0x7f) as usize;
            (
                d[2..2 + n]
                    .iter()
                    .fold(0usize, |a, b| (a << 8) | *b as usize),
                2 + n,
            )
        };
        (tag, &d[hdr..hdr + len], &d[hdr + len..])
    }
    let (tag, seq, _) = tlv(der);
    assert_eq!(tag, 0x30);
    let (_, n, rest) = tlv(seq);
    let (_, e, _) = tlv(rest);
    let strip = |v: &[u8]| {
        v.iter()
            .skip_while(|b| **b == 0)
            .cloned()
            .collect::<Vec<u8>>()
    };
    (strip(n), strip(e))
}

fn jwks_for(key: &RsaKeyPair, kid: &str) -> String {
    let (n, e) = rsa_components(key.public_key().as_ref());
    obj! { "keys" => steam_mcp::arr![obj! { "kty" => "RSA", "kid" => kid, "use" => "sig", "alg" => "RS256", "n" => b64url_encode(&n), "e" => b64url_encode(&e) }] }.to_string()
}

fn rs256(key: &RsaKeyPair, kid: &str, claims: &Value) -> String {
    let header = b64url_encode(
        obj! { "alg" => "RS256", "typ" => "at+jwt", "kid" => kid }
            .to_string()
            .as_bytes(),
    );
    let payload = b64url_encode(claims.to_string().as_bytes());
    let input = format!("{header}.{payload}");
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{input}.{}", b64url_encode(&sig))
}

fn cfg(pairs: &[(&str, &str)]) -> Config {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Config::from_map(&m).unwrap()
}

#[test]
fn rs256_tokens_validate_against_fetched_jwks() {
    let mock = start_mock();
    let key = test_key();
    *mock.jwks.lock().unwrap() = jwks_for(&key, "k1");
    let jwks_url = format!("{}/jwks", mock.base);
    let c = cfg(&[
        ("STEAM_MCP_AUTH_MODE", "jwt"),
        ("STEAM_MCP_JWKS_URL", &jwks_url),
        ("STEAM_MCP_PUBLIC_URL", RESOURCE),
        ("STEAM_MCP_AUTH_ISSUER", "https://auth.example.com"),
        (
            "STEAM_MCP_AUTHORIZATION_SERVERS",
            "https://auth.example.com",
        ),
        ("STEAM_MCP_USE_PROXY_ENV", "false"),
    ]);
    let auth = Authenticator::new(&c).unwrap();
    let claims = obj! { "sub" => "alice", "aud" => steam_mcp::arr![RESOURCE, "https://other"], "iss" => "https://auth.example.com", "exp" => now_secs() + 600, "scp" => steam_mcp::arr!["steam:financial"] };

    let p = auth
        .authenticate(&c, Some(&format!("Bearer {}", rs256(&key, "k1", &claims))))
        .unwrap();
    assert_eq!(p.id, "alice");
    assert!(
        p.has_scope("steam:financial")
            && p.has_scope("steam:public")
            && !p.has_scope("steam:partner")
    );

    // The JWKS is cached: a second validation does not refetch.
    auth.authenticate(&c, Some(&format!("Bearer {}", rs256(&key, "k1", &claims))))
        .unwrap();
    assert_eq!(mock.hits("/jwks"), 1);

    // Tampered payload, wrong audience, expired, unknown kid are all rejected.
    let token = rs256(&key, "k1", &claims);
    let mut parts: Vec<String> = token.split('.').map(String::from).collect();
    parts[1] = b64url_encode(claims.clone().with("sub", "mallory").to_string().as_bytes());
    assert!(
        auth.authenticate(&c, Some(&format!("Bearer {}", parts.join("."))))
            .is_err()
    );
    let wrong_aud = claims.clone().with("aud", "https://elsewhere.example.com");
    assert!(
        auth.authenticate(
            &c,
            Some(&format!("Bearer {}", rs256(&key, "k1", &wrong_aud)))
        )
        .is_err()
    );
    let expired = claims.clone().with("exp", now_secs() - 3600);
    assert!(
        auth.authenticate(&c, Some(&format!("Bearer {}", rs256(&key, "k1", &expired))))
            .is_err()
    );
    let unknown = auth
        .authenticate(&c, Some(&format!("Bearer {}", rs256(&key, "k2", &claims))))
        .unwrap_err();
    assert_eq!(unknown.error, Some("invalid_token"));
    // The unknown kid triggers one refresh, rate-limited thereafter.
    auth.authenticate(&c, Some(&format!("Bearer {}", rs256(&key, "k2", &claims))))
        .unwrap_err();
    assert_eq!(mock.hits("/jwks"), 2);
}

#[test]
fn introspection_with_client_credentials_and_caching() {
    let mock = start_mock();
    *mock.introspect_aud.lock().unwrap() = RESOURCE.into();
    let url = format!("{}/introspect", mock.base);
    let base = vec![
        ("STEAM_MCP_AUTH_MODE", "introspection"),
        ("STEAM_MCP_INTROSPECTION_URL", url.as_str()),
        ("STEAM_MCP_INTROSPECTION_CLIENT_ID", "mcp-client"),
        ("STEAM_MCP_PUBLIC_URL", RESOURCE),
        ("STEAM_MCP_AUTH_ISSUER", "https://auth.example.com"),
        (
            "STEAM_MCP_AUTHORIZATION_SERVERS",
            "https://auth.example.com",
        ),
        ("STEAM_MCP_USE_PROXY_ENV", "false"),
    ];
    let mut good = base.clone();
    good.push(("STEAM_MCP_INTROSPECTION_CLIENT_SECRET", "s3cret"));
    let c = cfg(&good);
    let auth = Authenticator::new(&c).unwrap();

    let p = auth.authenticate(&c, Some("Bearer good-token")).unwrap();
    assert_eq!(p.id, "svc-account");
    assert!(p.has_scope("steam:partner"));
    auth.authenticate(&c, Some("Bearer good-token")).unwrap();
    assert_eq!(mock.hits("/introspect"), 1, "active results are cached");

    assert!(auth.authenticate(&c, Some("Bearer revoked-token")).is_err());
    assert!(auth.authenticate(&c, Some("Bearer revoked-token")).is_err());
    assert_eq!(
        mock.hits("/introspect"),
        2,
        "inactive results are cached briefly"
    );

    assert!(
        auth.authenticate(&c, Some("Bearer other-aud")).is_err(),
        "audience must match this resource"
    );

    // Wrong client credentials: the authorization server refuses introspection.
    let mut bad = base.clone();
    bad.push(("STEAM_MCP_INTROSPECTION_CLIENT_SECRET", "wrong"));
    let c2 = cfg(&bad);
    let auth2 = Authenticator::new(&c2).unwrap();
    let e = auth2
        .authenticate(&c2, Some("Bearer good-token"))
        .unwrap_err();
    assert_eq!(e.status, 401);
}
