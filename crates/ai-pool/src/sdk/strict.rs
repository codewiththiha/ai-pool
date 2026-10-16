//! Rewrites a JSON Schema for strict mode: objects closed, properties
//! required, optional ones nullable.

use serde_json::{Map, Value, json};

/// Deeper schemas are left as they are rather than risk the stack.
const MAX_DEPTH: usize = 64;

/// Closes every object and turns optional properties into nullable ones.
pub fn make_strict(schema: &mut Value) {
    visit(schema, 0);
}

/// A schema name the API accepts: `[A-Za-z0-9_-]{1,64}`.
pub fn schema_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if clean.is_empty() {
        "response".into()
    } else {
        clean
    }
}

fn visit(node: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(obj) = node.as_object_mut() else {
        if let Some(list) = node.as_array_mut() {
            for item in list {
                visit(item, depth + 1);
            }
        }
        return;
    };
    for key in ["items", "anyOf", "oneOf", "allOf", "not", "prefixItems"] {
        if let Some(child) = obj.get_mut(key) {
            visit(child, depth + 1);
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get_mut(key) {
            defs.values_mut().for_each(|d| visit(d, depth + 1));
        }
    }
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        props.values_mut().for_each(|p| visit(p, depth + 1));
    }
    if is_object_schema(obj) {
        close(obj);
    }
}

fn is_object_schema(obj: &Map<String, Value>) -> bool {
    obj.contains_key("properties")
        || match obj.get("type") {
            Some(Value::String(t)) => t == "object",
            Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
            _ => false,
        }
}

fn close(obj: &mut Map<String, Value>) {
    obj.insert("additionalProperties".into(), Value::Bool(false));
    let required: Vec<String> = match obj.get("required") {
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let Some(Value::Object(props)) = obj.get_mut("properties") else {
        obj.insert("properties".into(), json!({}));
        obj.insert("required".into(), json!([]));
        return;
    };
    let mut all = Vec::with_capacity(props.len());
    for (name, prop) in props.iter_mut() {
        if !required.contains(name) {
            make_nullable(prop);
        }
        all.push(Value::String(name.clone()));
    }
    obj.insert("required".into(), Value::Array(all));
}

fn make_nullable(prop: &mut Value) {
    let Some(obj) = prop.as_object_mut() else {
        return;
    };
    match obj.get_mut("type") {
        Some(Value::String(t)) if t == "null" => {}
        Some(Value::String(t)) => {
            let t = std::mem::take(t);
            obj.insert("type".into(), json!([t, "null"]));
            if let Some(Value::Array(choices)) = obj.get_mut("enum")
                && !choices.contains(&Value::Null)
            {
                choices.push(Value::Null);
            }
        }
        Some(Value::Array(ts)) => {
            if !ts.iter().any(|t| t == "null") {
                ts.push(json!("null"));
            }
        }
        _ => {
            let inner = std::mem::take(prop);
            *prop = json!({ "anyOf": [inner, { "type": "null" }] });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_objects_close_and_optionals_become_nullable() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "word": { "type": "string" },
                "note": { "type": "string", "enum": ["a", "b"] },
                "meta": {
                    "type": "object",
                    "properties": { "level": { "$ref": "#/$defs/Level" } }
                },
                "tags": { "type": "array", "items": {
                    "type": "object", "properties": { "t": { "type": ["string", "null"] } }
                }}
            },
            "required": ["word", "meta", "tags"],
            "$defs": { "Level": { "type": "object", "properties": {} } }
        });
        make_strict(&mut schema);
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["word", "note", "meta", "tags"]));
        assert_eq!(
            schema["properties"]["note"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            schema["properties"]["note"]["enum"],
            json!(["a", "b", null])
        );
        let meta = &schema["properties"]["meta"];
        assert_eq!(meta["additionalProperties"], false);
        assert_eq!(meta["properties"]["level"]["anyOf"][1]["type"], "null");
        let item = &schema["properties"]["tags"]["items"];
        assert_eq!(item["required"], json!(["t"]));
        assert_eq!(item["properties"]["t"]["type"], json!(["string", "null"]));
        assert_eq!(schema["$defs"]["Level"]["additionalProperties"], false);
    }

    #[test]
    fn strictness_is_idempotent() {
        let mut once = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        make_strict(&mut once);
        let mut twice = once.clone();
        make_strict(&mut twice);
        assert_eq!(once, twice);
    }

    #[test]
    fn names_are_sanitised() {
        assert_eq!(schema_name("word info!"), "word_info_");
        assert_eq!(schema_name(""), "response");
        assert_eq!(schema_name(&"x".repeat(80)).len(), 64);
    }
}
