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
