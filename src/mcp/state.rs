//! Integrity-protected `requestState` for Multi Round-Trip Requests.
//!
//! The token binds the payload to the originating method, a digest of the
//! salient request parameters, the authenticated principal and an expiry, and
//! is authenticated with HMAC-SHA256. Payloads are *not* encrypted, so they
//! must never carry secrets.
//!
//! Format: `v1.<base64url(json)>.<base64url(hmac)>`

use std::time::Duration;

use super::protocol::McpError;
use crate::json::{self, Value};
use crate::obj;
use crate::util::crypto::{ct_eq, hmac_sha256, random_id, sha256_hex};
use crate::util::encoding::{b64url_decode_strict, b64url_encode};
use crate::util::time::now_ms;

/// What a request state is bound to.
pub struct Binding<'a> {
    pub method: &'a str,
    /// Salient parameters (e.g. `{name, arguments}` for `tools/call`).
    pub salient: &'a Value,
    pub principal: &'a str,
}

impl Binding<'_> {
    fn digest(&self) -> String {
        sha256_hex(format!("{}\n{}", self.method, self.salient.canonical()).as_bytes())
    }
}

pub struct StateSealer {
    key: Vec<u8>,
}

impl StateSealer {
    pub fn new(key: Vec<u8>) -> StateSealer {
        StateSealer { key }
    }

    pub fn seal(&self, binding: &Binding, payload: Value, ttl: Duration) -> String {
        let body = obj! {
            "m" => binding.method,
            "d" => binding.digest(),
            "p" => sha256_hex(binding.principal.as_bytes()),
            "e" => now_ms() + ttl.as_millis() as u64,
            "n" => random_id(8),
            "x" => payload,
        };
        let encoded = b64url_encode(body.to_string().as_bytes());
        let signing_input = format!("v1.{encoded}");
        let mac = hmac_sha256(&self.key, signing_input.as_bytes());
        format!("{signing_input}.{}", b64url_encode(&mac))
    }

    /// Verifies a state token and returns its payload.
    pub fn open(&self, token: &str, binding: &Binding) -> Result<Value, McpError> {
        let invalid = |why: &str| McpError::invalid_params(format!("Invalid requestState: {why}"));
        let mut parts = token.split('.');
        let (Some("v1"), Some(body), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid("malformed"));
        };
        let expected = hmac_sha256(&self.key, format!("v1.{body}").as_bytes());
        let given = b64url_decode_strict(mac).ok_or_else(|| invalid("malformed signature"))?;
        if !ct_eq(&expected, &given) {
            return Err(invalid("integrity check failed"));
        }
        let raw = b64url_decode_strict(body).ok_or_else(|| invalid("malformed body"))?;
        let v = json::parse_bytes(&raw).map_err(|_| invalid("malformed body"))?;
        if v["m"].as_str() != Some(binding.method)
            || v["d"].as_str() != Some(binding.digest().as_str())
        {
            return Err(invalid("state does not belong to this request"));
        }
        if v["p"].as_str() != Some(sha256_hex(binding.principal.as_bytes()).as_str()) {
            return Err(invalid("state was issued to a different principal"));
        }
        if v["e"].as_u64().is_none_or(|e| now_ms() > e) {
            return Err(invalid(
                "state has expired; retry the original request without requestState",
            ));
        }
        Ok(v.get("x").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open() {
        let s = StateSealer::new(b"k".to_vec());
        let args = obj! { "name" => "t", "arguments" => obj! { "appid" => 570 } };
        let b = Binding {
            method: "tools/call",
            salient: &args,
            principal: "alice",
        };
        let token = s.seal(&b, obj! { "step" => 1 }, Duration::from_secs(60));
        assert_eq!(s.open(&token, &b).unwrap()["step"].as_i64(), Some(1));

        // Tampering, wrong principal, wrong request, wrong key, expiry.
        let mut tampered = token.clone();
        tampered.insert(5, 'A');
        assert!(s.open(&tampered, &b).is_err());
        assert!(
            s.open(
                &token,
                &Binding {
                    principal: "bob",
                    ..b
                }
            )
            .is_err()
        );
        let other = obj! { "name" => "t", "arguments" => obj! { "appid" => 730 } };
        assert!(
            s.open(
                &token,
                &Binding {
                    method: "tools/call",
                    salient: &other,
                    principal: "alice"
                }
            )
            .is_err()
        );
        assert!(
            StateSealer::new(b"other".to_vec())
                .open(
                    &token,
                    &Binding {
                        method: "tools/call",
                        salient: &args,
                        principal: "alice"
                    }
                )
                .is_err()
        );
        let expired = s.seal(
            &Binding {
                method: "tools/call",
                salient: &args,
                principal: "alice",
            },
            Value::Null,
            Duration::ZERO,
        );
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            s.open(
                &expired,
                &Binding {
                    method: "tools/call",
                    salient: &args,
                    principal: "alice"
                }
            )
            .is_err()
        );
        assert!(
            s.open(
                "garbage",
                &Binding {
                    method: "tools/call",
                    salient: &args,
                    principal: "alice"
                }
            )
            .is_err()
        );
    }
}
