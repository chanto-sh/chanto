//! `--json-schema`: structured output for print mode. The model is asked to
//! finish with JSON matching the schema; the answer is checked here, and the
//! run asks once more with the errors when it doesn't match.
//!
//! The validator covers the JSON Schema keywords structured-output schemas
//! use: `type`, `properties`, `required`, `additionalProperties`, `items`,
//! `prefixItems`, `enum`, `const`, `anyOf`/`oneOf`/`allOf`/`not`, string,
//! number and array bounds, `pattern`-less strings, and local `$ref`s
//! (`#/$defs/…`, `#/definitions/…`). `format` is an annotation only.

use serde_json::Value;

const TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// Is `schema` a usable JSON Schema? (Claude Code exits on an invalid one.)
pub fn check_schema(schema: &Value) -> Result<(), String> {
    fn walk(s: &Value, at: &str) -> Result<(), String> {
        let o = match s {
            Value::Bool(_) => return Ok(()),
            Value::Object(o) => o,
            _ => return Err(format!("{at}: a schema must be an object or a boolean")),
        };
        match o.get("type") {
            None => {}
            Some(Value::String(t)) if TYPES.contains(&t.as_str()) => {}
            Some(Value::Array(a))
                if a.iter()
                    .all(|t| t.as_str().is_some_and(|t| TYPES.contains(&t))) => {}
            Some(t) => return Err(format!("{at}/type: {t} is not a JSON type")),
        }
        if let Some(r) = o.get("required")
            && !r.as_array().is_some_and(|a| a.iter().all(Value::is_string))
        {
            return Err(format!("{at}/required: must be an array of strings"));
        }
        if let Some(e) = o.get("enum")
            && !e.is_array()
        {
            return Err(format!("{at}/enum: must be an array"));
        }
        for k in ["properties", "$defs", "definitions", "patternProperties"] {
            match o.get(k) {
                None => {}
                Some(Value::Object(m)) => {
                    for (name, sub) in m {
                        walk(sub, &format!("{at}/{k}/{name}"))?;
                    }
                }
                Some(_) => return Err(format!("{at}/{k}: must be an object")),
            }
        }
        for k in ["items", "additionalProperties", "not", "contains"] {
            if let Some(sub) = o.get(k) {
                walk(sub, &format!("{at}/{k}"))?;
            }
        }
        for k in ["anyOf", "oneOf", "allOf", "prefixItems"] {
            match o.get(k) {
                None => {}
                Some(Value::Array(a)) => {
                    for (i, sub) in a.iter().enumerate() {
                        walk(sub, &format!("{at}/{k}/{i}"))?;
                    }
                }
                Some(_) => return Err(format!("{at}/{k}: must be an array")),
            }
        }
        if let Some(r) = o.get("$ref")
            && !r.as_str().is_some_and(|r| r.starts_with('#'))
        {
            return Err(format!(
                "{at}/$ref: only local references (#/…) are supported"
            ));
        }
        Ok(())
    }
    walk(schema, "#")
}

/// Every way `value` fails `schema` (empty: it matches).
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    check(schema, schema, value, "$", &mut errs, 0);
    errs
}

fn resolve<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let path = r.strip_prefix('#')?;
    if path.is_empty() {
        return Some(root);
    }
    root.pointer(&path.replace("~1", "/").replace("~0", "~"))
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => {
            v.as_i64().is_some()
                || v.as_u64().is_some()
                || v.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn check(root: &Value, s: &Value, v: &Value, at: &str, errs: &mut Vec<String>, depth: usize) {
    if depth > 64 {
        return;
    }
    let o = match s {
        Value::Bool(true) => return,
        Value::Bool(false) => return errs.push(format!("{at}: no value is allowed here")),
        Value::Object(o) => o,
        _ => return,
    };
    if let Some(r) = o.get("$ref").and_then(Value::as_str) {
        match resolve(root, r) {
            Some(target) => check(root, target, v, at, errs, depth + 1),
            None => errs.push(format!("{at}: unresolved $ref {r}")),
        }
    }
    match o.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => {
            return errs.push(format!("{at}: expected {t}, got {}", kind(v)));
        }
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)) => {
            return errs.push(format!(
                "{at}: expected one of {}, got {}",
                ts.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                kind(v)
            ));
        }
        _ => {}
    }
    if let Some(e) = o.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        errs.push(format!(
            "{at}: {v} is not one of {}",
            Value::Array(e.clone())
        ));
    }
    if let Some(c) = o.get("const")
        && c != v
    {
        errs.push(format!("{at}: must be {c}"));
    }
    for k in ["allOf", "anyOf", "oneOf"] {
        let Some(subs) = o.get(k).and_then(Value::as_array) else {
            continue;
        };
        let passing = subs
            .iter()
            .filter(|sub| validate_at(root, sub, v, at, depth).is_empty())
            .count();
        match k {
            "allOf" => {
                for sub in subs {
                    check(root, sub, v, at, errs, depth + 1);
                }
            }
            "anyOf" if passing == 0 => {
                errs.push(format!("{at}: matches none of the anyOf schemas"))
            }
            "oneOf" if passing != 1 => errs.push(format!(
                "{at}: must match exactly one oneOf schema (matches {passing})"
            )),
            _ => {}
        }
    }
    if let Some(n) = o.get("not")
        && validate_at(root, n, v, at, depth).is_empty()
    {
        errs.push(format!("{at}: must not match the `not` schema"));
    }
    match v {
        Value::Object(m) => {
            let props = o.get("properties").and_then(Value::as_object);
            for r in o
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !m.contains_key(r) {
                    errs.push(format!("{at}: missing required property `{r}`"));
                }
            }
            for (k, val) in m {
                let sub_at = format!("{at}.{k}");
                match props.and_then(|p| p.get(k)) {
                    Some(sub) => check(root, sub, val, &sub_at, errs, depth + 1),
                    None => match o.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            errs.push(format!("{at}: unexpected property `{k}`"))
                        }
                        Some(sub @ Value::Object(_)) => {
                            check(root, sub, val, &sub_at, errs, depth + 1)
                        }
                        _ => {}
                    },
                }
            }
            bound(
                o,
                "minProperties",
                m.len(),
                at,
                errs,
                |n, l| n >= l,
                "at least",
                "properties",
            );
            bound(
                o,
                "maxProperties",
                m.len(),
                at,
                errs,
                |n, l| n <= l,
                "at most",
                "properties",
            );
        }
        Value::Array(a) => {
            let prefix = o.get("prefixItems").and_then(Value::as_array);
            for (i, item) in a.iter().enumerate() {
                let sub = prefix.and_then(|p| p.get(i)).or_else(|| o.get("items"));
                if let Some(sub) = sub {
                    check(root, sub, item, &format!("{at}[{i}]"), errs, depth + 1);
                }
            }
            bound(
                o,
                "minItems",
                a.len(),
                at,
                errs,
                |n, l| n >= l,
                "at least",
                "items",
            );
            bound(
                o,
                "maxItems",
                a.len(),
                at,
                errs,
                |n, l| n <= l,
                "at most",
                "items",
            );
            if o.get("uniqueItems") == Some(&Value::Bool(true))
                && a.iter().enumerate().any(|(i, x)| a[..i].contains(x))
            {
                errs.push(format!("{at}: items must be unique"));
            }
        }
        Value::String(s) => {
            let n = s.chars().count();
            bound(
                o,
                "minLength",
                n,
                at,
                errs,
                |n, l| n >= l,
                "at least",
                "characters",
            );
            bound(
                o,
                "maxLength",
                n,
                at,
                errs,
                |n, l| n <= l,
                "at most",
                "characters",
            );
        }
        Value::Number(num) => {
            let x = num.as_f64().unwrap_or(0.0);
            let lim = |k: &str| o.get(k).and_then(Value::as_f64);
            if lim("minimum").is_some_and(|m| x < m)
                || lim("exclusiveMinimum").is_some_and(|m| x <= m)
            {
                errs.push(format!("{at}: {x} is below the minimum"));
            }
            if lim("maximum").is_some_and(|m| x > m)
                || lim("exclusiveMaximum").is_some_and(|m| x >= m)
            {
                errs.push(format!("{at}: {x} is above the maximum"));
            }
            if let Some(m) = lim("multipleOf").filter(|m| *m > 0.0)
                && ((x / m).round() * m - x).abs() > 1e-9
            {
                errs.push(format!("{at}: {x} is not a multiple of {m}"));
            }
        }
        _ => {}
    }
}

fn validate_at(root: &Value, s: &Value, v: &Value, at: &str, depth: usize) -> Vec<String> {
    let mut e = Vec::new();
    check(root, s, v, at, &mut e, depth + 1);
    e
}

#[allow(clippy::too_many_arguments)]
fn bound(
    o: &serde_json::Map<String, Value>,
    key: &str,
    n: usize,
    at: &str,
    errs: &mut Vec<String>,
    ok: fn(usize, usize) -> bool,
    word: &str,
    unit: &str,
) {
    if let Some(l) = o.get(key).and_then(Value::as_u64)
        && !ok(n, l as usize)
    {
        errs.push(format!("{at}: needs {word} {l} {unit} (has {n})"));
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The JSON in a final answer: the whole text, a fenced ```json block, or
/// the outermost `{…}` / `[…]` span.
pub fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Some(v);
    }
    if let Some(start) = t.find("```") {
        let rest = &t[start + 3..];
        let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or(rest);
        if let Some(end) = body.find("```")
            && let Ok(v) = serde_json::from_str(body[..end].trim())
        {
            return Some(v);
        }
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(a), Some(b)) = (t.find(open), t.rfind(close))
            && a < b
            && let Ok(v) = serde_json::from_str(&t[a..=b])
        {
            return Some(v);
        }
    }
    None
}

/// Added to the prompt: what the final answer must look like.
pub fn instruction(schema: &Value) -> String {
    format!(
        "<system-reminder>When you have finished the task, your final message must be only a JSON value that matches this JSON Schema (no prose, no code fence):\n{schema}\n</system-reminder>"
    )
}

/// The follow-up when the answer didn't match.
pub fn repair_prompt(schema: &Value, problems: &[String]) -> String {
    format!(
        "Your final answer is not valid structured output:\n- {}\n\nReply with only a JSON value that matches this JSON Schema (no prose, no code fence):\n{schema}",
        problems.join("\n- ")
    )
}

/// Check a final answer: the value, or what's wrong with it.
pub fn parse_answer(schema: &Value, text: &str) -> Result<Value, Vec<String>> {
    let Some(v) = extract_json(text) else {
        return Err(vec!["the answer contains no JSON value".into()]);
    };
    let errs = validate(schema, &v);
    if errs.is_empty() { Ok(v) } else { Err(errs) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schemas_are_checked() {
        assert!(check_schema(&json!({"type": "object", "properties": {"a": {"type": "string", "format": "email"}}, "required": ["a"]})).is_ok());
        assert!(check_schema(&json!({"type": "objekt"})).is_err());
        assert!(check_schema(&json!({"properties": {"a": 3}})).is_err());
        assert!(check_schema(&json!({"required": "a"})).is_err());
        assert!(check_schema(&json!("string")).is_err());
        assert!(check_schema(&json!({"$ref": "http://x/s.json"})).is_err());
    }

    #[test]
    fn values_are_validated() {
        let s = json!({
            "type": "object",
            "properties": {
                "functions": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "count": {"type": "integer", "minimum": 0},
                "level": {"enum": ["low", "high"]},
                "item": {"$ref": "#/$defs/item"}
            },
            "required": ["functions"],
            "additionalProperties": false,
            "$defs": {"item": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}
        });
        assert!(
            validate(
                &s,
                &json!({"functions": ["a"], "count": 2, "level": "low", "item": {"id": "x"}})
            )
            .is_empty()
        );
        let e = validate(
            &s,
            &json!({"functions": [], "count": -1.5, "level": "mid", "extra": 1, "item": {}}),
        );
        let all = e.join("\n");
        for want in [
            "needs at least 1 items",
            "$.count: expected integer",
            "is not one of",
            "unexpected property `extra`",
            "missing required property `id`",
        ] {
            assert!(all.contains(want), "{want} not in {all}");
        }
        assert!(
            validate(&s, &json!({}))
                .iter()
                .any(|e| e.contains("missing required property `functions`"))
        );
        assert!(
            validate(
                &json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
                &json!(null)
            )
            .is_empty()
        );
        assert!(
            !validate(
                &json!({"oneOf": [{"type": "number"}, {"type": "integer"}]}),
                &json!(3)
            )
            .is_empty()
        );
    }

    #[test]
    fn answers_are_extracted_from_prose_and_fences() {
        assert_eq!(extract_json("{\"a\":1}"), Some(json!({"a": 1})));
        assert_eq!(
            extract_json("Here:\n```json\n{\"a\": 2}\n```\nDone."),
            Some(json!({"a": 2}))
        );
        assert_eq!(
            extract_json("The result is {\"a\": 3}."),
            Some(json!({"a": 3}))
        );
        assert_eq!(extract_json("no json here"), None);
        let s = json!({"type": "object", "required": ["a"]});
        assert!(parse_answer(&s, "{\"a\": 1}").is_ok());
        assert!(parse_answer(&s, "{\"b\": 1}").is_err());
    }
}
