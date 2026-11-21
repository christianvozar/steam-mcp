//! A bounded JSON Schema (2020-12 subset) validator with lenient coercion.
//!
//! Supports: `type` (string or array), `enum`, `const`, `properties`,
//! `required`, `additionalProperties` (bool or schema), `items`, `minItems`,
//! `maxItems`, `uniqueItems`, `minimum`, `maximum`, `exclusiveMinimum`,
//! `exclusiveMaximum`, `minLength`, `maxLength`, `format` (date, date-time,
//! email, uri), `anyOf`, `oneOf`, `allOf`, `not`, and local `$ref`
//! (`#/$defs/...`). Network `$ref`s are never dereferenced. Depth and
//! subschema-count bounds protect against pathological schemas.

use crate::json::{Number, Value};

const MAX_DEPTH: usize = 32;
const MAX_STEPS: usize = 20_000;

struct Ctx<'a> {
    root: &'a Value,
    errors: Vec<String>,
    steps: usize,
}

/// Validates `value` against `schema`, returning human-readable errors.
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    if let Some(d) = schema.get("$schema").and_then(|s| s.as_str()) {
        if !(d.contains("2020-12") || d.contains("draft-07") || d.contains("2019-09")) {
            return Err(vec![format!("unsupported JSON Schema dialect: {d}")]);
        }
    }
    let mut ctx = Ctx {
        root: schema,
        errors: Vec::new(),
        steps: 0,
    };
    check(&mut ctx, schema, value, "$", 0);
    if ctx.errors.is_empty() {
        Ok(())
    } else {
        Err(ctx.errors)
    }
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "null" => v.is_null(),
        "boolean" => matches!(v, Value::Bool(_)),
        "integer" => matches!(v, Value::Number(n) if n.is_integer()),
        "number" => v.is_number(),
        "string" => v.is_string(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => false,
    }
}

fn resolve<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let path = r.strip_prefix("#/")?;
    let mut cur = root;
    for seg in path.split('/') {
        let seg = seg.replace("~1", "/").replace("~0", "~");
        cur = cur.get(&seg)?;
    }
    Some(cur)
}

fn check(ctx: &mut Ctx, schema: &Value, v: &Value, path: &str, depth: usize) {
    ctx.steps += 1;
    if depth > MAX_DEPTH || ctx.steps > MAX_STEPS {
        ctx.errors
            .push(format!("{path}: schema too complex to validate"));
        return;
    }
    let s = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            ctx.errors.push(format!("{path}: no value is allowed here"));
            return;
        }
        Value::Object(m) => m,
        _ => return,
    };
    if let Some(r) = s.get("$ref").and_then(|r| r.as_str()) {
        match resolve(ctx.root, r) {
            Some(target) => check(ctx, target, v, path, depth + 1),
            None => ctx
                .errors
                .push(format!("{path}: unresolvable or non-local $ref {r}")),
        }
    }
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(t) => type_matches(t, v),
            Value::Array(ts) => ts
                .iter()
                .filter_map(|t| t.as_str())
                .any(|t| type_matches(t, v)),
            _ => true,
        };
        if !ok {
            ctx.errors.push(format!(
                "{path}: expected {}, got {}",
                t.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| t.to_string()),
                v.type_name()
            ));
            return;
        }
    }
    if let Some(Value::Array(options)) = s.get("enum") {
        if !options.contains(v) {
            let list: Vec<String> = options.iter().map(|o| o.to_string()).collect();
            ctx.errors
                .push(format!("{path}: must be one of {}", list.join(", ")));
        }
    }
    if let Some(c) = s.get("const") {
        if c != v {
            ctx.errors.push(format!("{path}: must equal {c}"));
        }
    }
    match v {
        Value::Number(n) => check_number(ctx, s, n, path),
        Value::String(st) => {
            let len = st.chars().count() as u64;
            if let Some(min) = s.get("minLength").and_then(|x| x.as_u64()) {
                if len < min {
                    ctx.errors
                        .push(format!("{path}: must be at least {min} characters"));
                }
            }
            if let Some(max) = s.get("maxLength").and_then(|x| x.as_u64()) {
                if len > max {
                    ctx.errors
                        .push(format!("{path}: must be at most {max} characters"));
                }
            }
            if let Some(fmt) = s.get("format").and_then(|f| f.as_str()) {
                let ok = match fmt {
                    "date" => st.len() == 10 && crate::util::time::parse_datetime(st).is_some(),
                    "date-time" => st.len() > 10 && crate::util::time::parse_datetime(st).is_some(),
                    "email" => st.contains('@') && !st.starts_with('@') && !st.ends_with('@'),
                    "uri" => st.contains(':') && !st.contains(' '),
                    _ => true,
                };
                if !ok {
                    ctx.errors.push(format!("{path}: must be a valid {fmt}"));
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = s.get("minItems").and_then(|x| x.as_u64()) {
                if (items.len() as u64) < min {
                    ctx.errors
                        .push(format!("{path}: must contain at least {min} items"));
                }
            }
            if let Some(max) = s.get("maxItems").and_then(|x| x.as_u64()) {
                if (items.len() as u64) > max {
                    ctx.errors
                        .push(format!("{path}: must contain at most {max} items"));
                }
            }
            if s.get("uniqueItems").and_then(|x| x.as_bool()) == Some(true) {
                for i in 0..items.len() {
                    if items[i + 1..].contains(&items[i]) {
                        ctx.errors.push(format!("{path}: items must be unique"));
                        break;
                    }
                }
            }
            if let Some(item_schema) = s.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(ctx, item_schema, item, &format!("{path}[{i}]"), depth + 1);
                }
            }
        }
        Value::Object(obj) => {
            if let Some(Value::Array(req)) = s.get("required") {
                for r in req.iter().filter_map(|r| r.as_str()) {
                    if !obj.contains_key(r) {
                        ctx.errors
                            .push(format!("{path}: missing required property '{r}'"));
                    }
                }
            }
            let props = s.get("properties").and_then(|p| p.as_object());
            for (k, val) in obj.iter() {
                let child = format!("{path}.{k}");
                match props.and_then(|p| p.get(k)) {
                    Some(ps) => check(ctx, ps, val, &child, depth + 1),
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            ctx.errors.push(format!("{path}: unknown property '{k}'"))
                        }
                        Some(ap @ Value::Object(_)) => check(ctx, ap, val, &child, depth + 1),
                        _ => {}
                    },
                }
            }
        }
        _ => {}
    }
    for (kw, mode) in [("allOf", 0), ("anyOf", 1), ("oneOf", 2)] {
        if let Some(Value::Array(subs)) = s.get(kw) {
            let mut passed = 0;
            for sub in subs {
                let mut sub_ctx = Ctx {
                    root: ctx.root,
                    errors: Vec::new(),
                    steps: ctx.steps,
                };
                check(&mut sub_ctx, sub, v, path, depth + 1);
                ctx.steps = sub_ctx.steps;
                if sub_ctx.errors.is_empty() {
                    passed += 1;
                } else if mode == 0 {
                    ctx.errors.extend(sub_ctx.errors);
                }
            }
            match mode {
                1 if passed == 0 => ctx
                    .errors
                    .push(format!("{path}: does not match any allowed schema")),
                2 if passed != 1 => ctx.errors.push(format!(
                    "{path}: must match exactly one schema (matched {passed})"
                )),
                _ => {}
            }
        }
    }
    if let Some(not) = s.get("not") {
        let mut sub_ctx = Ctx {
            root: ctx.root,
            errors: Vec::new(),
            steps: ctx.steps,
        };
        check(&mut sub_ctx, not, v, path, depth + 1);
        ctx.steps = sub_ctx.steps;
        if sub_ctx.errors.is_empty() {
            ctx.errors
                .push(format!("{path}: must not match the excluded schema"));
        }
    }
}

fn check_number(ctx: &mut Ctx, s: &crate::json::Map, n: &Number, path: &str) {
    let x = n.as_f64();
    if let Some(min) = s.get("minimum").and_then(|m| m.as_f64()) {
        if x < min {
            ctx.errors.push(format!("{path}: must be >= {min}"));
        }
    }
    if let Some(max) = s.get("maximum").and_then(|m| m.as_f64()) {
        if x > max {
            ctx.errors.push(format!("{path}: must be <= {max}"));
        }
    }
    if let Some(min) = s.get("exclusiveMinimum").and_then(|m| m.as_f64()) {
        if x <= min {
            ctx.errors.push(format!("{path}: must be > {min}"));
        }
    }
    if let Some(max) = s.get("exclusiveMaximum").and_then(|m| m.as_f64()) {
        if x >= max {
            ctx.errors.push(format!("{path}: must be < {max}"));
        }
    }
}

/// Coerces common LLM argument mistakes in place, guided by the schema:
/// numeric strings for integers/numbers, "true"/"false" strings for
/// booleans, and comma-separated strings or scalars for arrays.
pub fn coerce(schema: &Value, v: &mut Value) {
    coerce_inner(schema, v, 0)
}

fn primary_type(schema: &Value) -> Option<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => Some(t),
        Some(Value::Array(ts)) => ts.iter().filter_map(|t| t.as_str()).find(|t| *t != "null"),
        _ => None,
    }
}

fn coerce_inner(schema: &Value, v: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(t) = primary_type(schema) else {
        return;
    };
    match (t, &*v) {
        ("integer", Value::String(s)) => {
            let s = s.trim();
            if let Ok(i) = s.parse::<i64>() {
                *v = Value::from(i);
            } else if let Ok(u) = s.parse::<u64>() {
                *v = Value::from(u);
            }
        }
        ("number", Value::String(s)) => {
            if let Ok(f) = s.trim().parse::<f64>() {
                *v = Value::from(f);
            }
        }
        ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => *v = Value::Bool(true),
            "false" | "0" | "no" => *v = Value::Bool(false),
            _ => {}
        },
        ("boolean", Value::Number(n)) if n.is_integer() => *v = Value::Bool(n.as_i64() != Some(0)),
        ("string", Value::Number(_)) => *v = Value::String(v.to_string()),
        ("array", Value::String(s)) => {
            let parts: Vec<Value> = s
                .split(',')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(Value::from)
                .collect();
            *v = Value::Array(parts);
        }
        ("array", Value::Number(_)) => *v = Value::Array(vec![v.clone()]),
        _ => {}
    }
    match v {
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items.iter_mut() {
                    coerce_inner(item_schema, item, depth + 1);
                }
            }
        }
        Value::Object(obj) => {
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for (k, ps) in props.iter() {
                    if let Some(child) = obj.get_mut(k) {
                        coerce_inner(ps, child, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn validates_common_keywords() {
        let schema = parse(
            r##"{
            "type":"object",
            "properties":{
                "appid":{"type":"integer","minimum":1},
                "cc":{"type":"string","minLength":2,"maxLength":2},
                "filter":{"type":"string","enum":["recent","all"]},
                "ids":{"type":"array","items":{"type":"integer"},"maxItems":2,"uniqueItems":true},
                "date":{"type":"string","format":"date"},
                "ref":{"$ref":"#/$defs/pos"}
            },
            "$defs":{"pos":{"type":"number","exclusiveMinimum":0}},
            "required":["appid"],
            "additionalProperties":false
        }"##,
        )
        .unwrap();
        assert!(validate(&schema, &parse(r#"{"appid":570,"cc":"us","filter":"all","ids":[1,2],"date":"2026-01-31","ref":0.5}"#).unwrap()).is_ok());
        let errs = validate(&schema, &parse(r#"{"appid":0,"cc":"usa","filter":"x","ids":[1,1,3],"date":"2026-02-31","ref":0,"extra":1}"#).unwrap()).unwrap_err();
        assert_eq!(errs.len(), 8, "{errs:?}");
        assert!(validate(&schema, &parse("{}").unwrap()).is_err());
        assert!(validate(&schema, &parse("[]").unwrap()).is_err());
    }

    #[test]
    fn composition() {
        let s = parse(r#"{"oneOf":[{"type":"string"},{"type":"integer"}]}"#).unwrap();
        assert!(validate(&s, &Value::from(1)).is_ok());
        assert!(validate(&s, &Value::from(true)).is_err());
        let s = parse(r#"{"not":{"type":"null"}}"#).unwrap();
        assert!(validate(&s, &Value::Null).is_err());
        let s = parse(r#"{"$ref":"https://evil.example/schema"}"#).unwrap();
        assert!(validate(&s, &Value::from(1)).is_err());
        let s = parse(r#"{"$schema":"https://example.com/custom","type":"object"}"#).unwrap();
        assert!(validate(&s, &parse("{}").unwrap()).is_err());
    }

    #[test]
    fn coercion() {
        let schema = parse(r#"{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"boolean"},"c":{"type":"array","items":{"type":"integer"}},"d":{"type":"string"}}}"#).unwrap();
        let mut v = parse(r#"{"a":"570","b":"true","c":"1, 2,3","d":5}"#).unwrap();
        coerce(&schema, &mut v);
        assert_eq!(v.to_string(), r#"{"a":570,"b":true,"c":[1,2,3],"d":"5"}"#);
        assert!(validate(&schema, &v).is_ok());
    }
}
