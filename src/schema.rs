//! Turns the caller's "entities to extract" JSON into a strict JSON Schema.
//!
//! Two input styles are accepted:
//!
//! 1. A real JSON Schema (`{"type": "object", "properties": {...}}`). It is
//!    normalised so every object has `additionalProperties: false` and lists all
//!    of its properties as required, which the strict modes of all three
//!    providers demand.
//! 2. A plain JSON template whose values describe the expected type, e.g.
//!    `{"invoice_number": "", "total": 0.0, "paid": false,
//!      "line_items": [{"description": "string", "amount": "number: incl. VAT"}]}`.
//!    A string leaf may start with a type word (`string`, `number`, `integer`,
//!    `boolean`, `date`) optionally followed by a description.

use serde_json::{Map, Value, json};

pub fn looks_like_json_schema(v: &Value) -> bool {
    match v.as_object() {
        Some(o) => {
            o.contains_key("$schema")
                || (o.get("type").and_then(Value::as_str) == Some("object")
                    && o.contains_key("properties"))
        }
        None => false,
    }
}

/// Build the schema sent to the model from whatever the caller gave us.
pub fn build_schema(input: &Value, nullable_leaves: bool) -> Result<Value, String> {
    if looks_like_json_schema(input) {
        let mut s = input.clone();
        normalize_schema(&mut s);
        return Ok(s);
    }
    if !input.is_object() {
        return Err("the schema/template must be a JSON object".into());
    }
    Ok(template_to_schema(input, nullable_leaves))
}

fn template_to_schema(v: &Value, nullable: bool) -> Value {
    match v {
        Value::Object(fields) => {
            let mut props = Map::new();
            let mut required = Vec::new();
            for (k, fv) in fields {
                props.insert(k.clone(), template_to_schema(fv, nullable));
                required.push(Value::String(k.clone()));
            }
            json!({
                "type": "object",
                "properties": Value::Object(props),
                "required": required,
                "additionalProperties": false
            })
        }
        Value::Array(items) => {
            let item_schema = match items.first() {
                Some(first) => template_to_schema(first, nullable),
                None => leaf("string", None, nullable),
            };
            json!({ "type": "array", "items": item_schema })
        }
        Value::String(s) => {
            let (ty, desc) = parse_type_hint(s);
            leaf(ty, desc, nullable)
        }
        Value::Number(n) => leaf(
            if n.is_f64() { "number" } else { "integer" },
            None,
            nullable,
        ),
        Value::Bool(_) => leaf("boolean", None, nullable),
        Value::Null => leaf("string", None, true),
    }
}

fn leaf(ty: &str, description: Option<&str>, nullable: bool) -> Value {
    let mut m = Map::new();
    if nullable {
        m.insert("type".into(), json!([ty, "null"]));
    } else {
        m.insert("type".into(), json!(ty));
    }
    if let Some(d) = description {
        m.insert("description".into(), json!(d));
    }
    Value::Object(m)
}

/// `"number: total incl. VAT"` -> ("number", Some("total incl. VAT")).
/// A string with no recognised prefix is a plain string field whose text, if
/// any, becomes its description.
fn parse_type_hint(s: &str) -> (&'static str, Option<&str>) {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return ("string", None);
    }
    let (word, rest) = match trimmed
        .find(|c: char| c.is_whitespace() || matches!(c, ':' | '-' | '|' | '—' | '–'))
    {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => (trimmed, ""),
    };
    let ty = match word.to_ascii_lowercase().as_str() {
        "string" | "str" | "text" => "string",
        "number" | "float" | "decimal" | "money" | "currency" => "number",
        "integer" | "int" => "integer",
        "boolean" | "bool" => "boolean",
        "date" | "datetime" | "time" => "string",
        _ => return ("string", Some(trimmed)),
    };
    let desc = rest
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '-' | '|' | '—' | '–'))
        .trim();
    let desc = if word.eq_ignore_ascii_case("date") && desc.is_empty() {
        Some("ISO 8601 date (YYYY-MM-DD)")
    } else if desc.is_empty() {
        None
    } else {
        Some(desc)
    };
    (ty, desc)
}

/// Make a user-supplied JSON Schema acceptable to strict structured-output modes.
fn normalize_schema(v: &mut Value) {
    let Some(obj) = v.as_object_mut() else { return };
    let is_object_schema =
        obj.get("type").and_then(Value::as_str) == Some("object") || obj.contains_key("properties");
    if is_object_schema {
        obj.entry("additionalProperties")
            .or_insert(Value::Bool(false));
        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            let keys: Vec<Value> = props.keys().cloned().map(Value::String).collect();
            obj.insert("required".into(), Value::Array(keys));
        }
    }
    if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
        for p in props.values_mut() {
            normalize_schema(p);
        }
    }
    if let Some(items) = obj.get_mut("items") {
        normalize_schema(items);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(arr) = obj.get_mut(key).and_then(Value::as_array_mut) {
            for s in arr {
                normalize_schema(s);
            }
        }
    }
    if let Some(defs) = obj.get_mut("$defs").and_then(Value::as_object_mut) {
        for d in defs.values_mut() {
            normalize_schema(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_schema_from_template() {
        let template = json!({
            "invoice_number": "",
            "total": 0.0,
            "count": 3,
            "paid": false,
            "issued": "date",
            "vendor": {"name": "string: legal name", "vat_id": null},
            "line_items": [{"description": "", "amount": "number"}]
        });
        let s = build_schema(&template, true).unwrap();
        assert_eq!(s["type"], "object");
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["properties"]["total"]["type"], json!(["number", "null"]));
        assert_eq!(s["properties"]["count"]["type"], json!(["integer", "null"]));
        assert_eq!(s["properties"]["paid"]["type"], json!(["boolean", "null"]));
        assert_eq!(
            s["properties"]["issued"]["description"],
            "ISO 8601 date (YYYY-MM-DD)"
        );
        assert_eq!(
            s["properties"]["vendor"]["properties"]["name"]["description"],
            "legal name"
        );
        assert_eq!(
            s["properties"]["line_items"]["items"]["properties"]["amount"]["type"],
            json!(["number", "null"])
        );
        assert_eq!(s["required"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn non_nullable_leaves() {
        let s = build_schema(&json!({"a": ""}), false).unwrap();
        assert_eq!(s["properties"]["a"]["type"], "string");
    }

    #[test]
    fn passes_through_and_normalizes_real_schema() {
        let input = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "object", "properties": {"k": {"type": "string"}}}}
            }
        });
        let s = build_schema(&input, true).unwrap();
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["required"], json!(["name", "tags"]));
        assert_eq!(
            s["properties"]["tags"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(s["properties"]["tags"]["items"]["required"], json!(["k"]));
    }

    #[test]
    fn rejects_non_object() {
        assert!(build_schema(&json!([1, 2]), true).is_err());
    }
}
