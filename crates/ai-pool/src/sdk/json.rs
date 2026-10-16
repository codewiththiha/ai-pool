//! JSON in model text: whole answers, answers mid-stream, schema checks.

use serde_json::{Map, Value};

/// How many cut points a partial answer tries before giving up.
const MAX_CUTS: usize = 8;
/// Deeper schemas and values are trusted rather than risk the stack.
const MAX_DEPTH: usize = 64;

/// The JSON value in `text`: bare, fenced, after `<think>`, or amid prose.
pub fn extract(text: &str) -> Option<Value> {
    let text = strip_fences(after_thinking(text)?);
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    let start = text.find(['{', '['])?;
    let closer = if text.get(start..)?.starts_with('{') {
        '}'
    } else {
        ']'
    };
    let end = text.rfind(closer)?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

/// What a prefix of an object says so far; open strings and brackets close.
pub fn partial(text: &str) -> Option<Value> {
    let text = after_thinking(text)?;
    let body = text.get(text.find('{')?..)?;
    let scan = Scan::of(body);
    if let Some(v) = close(body, scan.end.as_ref()) {
        return Some(v);
    }
    // The last member may be a bare key or a cut number, so it goes.
    scan.cuts
        .iter()
        .rev()
        .take(MAX_CUTS)
        .find_map(|(at, open)| close(body.get(..*at)?, Some(open)))
}

/// Checks types, required keys and enums; extra keys and lengths pass.
pub fn check(value: &Value, schema: &Value) -> Result<(), String> {
    visit(value, schema, "$", 0)
}

/// `schema` as an example object, each value a placeholder.
pub fn skeleton(schema: &Value) -> Value {
    sketch(schema, 0)
}

fn visit(value: &Value, schema: &Value, at: &str, depth: usize) -> Result<(), String> {
    let Some(s) = schema.as_object() else {
        return Ok(());
    };
    if depth > MAX_DEPTH {
        return Ok(());
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = s.get(key)
            && !branches.is_empty()
            && !branches
                .iter()
                .any(|b| visit(value, b, at, depth + 1).is_ok())
        {
            return Err(format!("{at} matches none of the choices"));
        }
    }
    if let Some(Value::Array(choices)) = s.get("enum")
        && !choices.contains(value)
    {
        return Err(format!("{at} is not one of the allowed values"));
    }
    if !type_fits(value, s.get("type")) {
        return Err(format!("{at} is {}, not the expected type", kind(value)));
    }
    match value {
        Value::Object(fields) => fields_fit(fields, s, at, depth),
        Value::Array(items) => s.get("items").map_or(Ok(()), |item| {
            items
                .iter()
                .enumerate()
                .try_for_each(|(i, v)| visit(v, item, &format!("{at}[{i}]"), depth + 1))
        }),
        _ => Ok(()),
    }
}

fn fields_fit(
    fields: &Map<String, Value>,
    s: &Map<String, Value>,
    at: &str,
    depth: usize,
) -> Result<(), String> {
    if let Some(Value::Array(required)) = s.get("required") {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|k| !fields.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            let has: Vec<&str> = fields.keys().map(String::as_str).collect();
            return Err(format!(
                "{at} lacks {}; it has {}",
                missing.join(", "),
                if has.is_empty() {
                    "no keys".to_owned()
                } else {
                    has.join(", ")
                }
            ));
        }
    }
    let Some(Value::Object(props)) = s.get("properties") else {
        return Ok(());
    };
    props.iter().try_for_each(|(k, p)| {
        fields
            .get(k)
            .map_or(Ok(()), |v| visit(v, p, &format!("{at}.{k}"), depth + 1))
    })
}

fn type_fits(value: &Value, want: Option<&Value>) -> bool {
    match want {
        Some(Value::String(t)) => one_type_fits(value, t),
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .any(|t| one_type_fits(value, t)),
        _ => true,
    }
}

fn one_type_fits(value: &Value, t: &str) -> bool {
    match (t, value) {
        ("string", Value::String(_))
        | ("number", Value::Number(_))
        | ("boolean", Value::Bool(_))
        | ("array", Value::Array(_))
        | ("object", Value::Object(_))
        | ("null", Value::Null) => true,
        ("integer", Value::Number(n)) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

const fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn sketch(schema: &Value, depth: usize) -> Value {
    let Some(s) = schema.as_object() else {
        return Value::Null;
    };
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    if let Some(Value::Array(choices)) = s.get("enum")
        && let Some(first) = choices.first()
    {
        return first.clone();
    }
    let t = match s.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("null"),
        _ if s.contains_key("properties") => "object",
        _ => "",
    };
    match t {
        "object" => {
            let mut out = Map::new();
            if let Some(Value::Object(props)) = s.get("properties") {
                for (k, p) in props {
                    out.insert(k.clone(), sketch(p, depth + 1));
                }
            }
            Value::Object(out)
        }
        "array" => {
            let item = s.get("items").map_or(Value::Null, |i| sketch(i, depth + 1));
            Value::Array(vec![item])
        }
        "string" => Value::String(placeholder(s)),
        "number" | "integer" => Value::from(0),
        "boolean" => Value::Bool(false),
        _ => Value::Null,
    }
}

/// A string field's placeholder: its description, else `"..."`.
fn placeholder(s: &Map<String, Value>) -> String {
    s.get("description")
        .and_then(Value::as_str)
        .map_or_else(|| "...".to_owned(), |d| format!("<{d}>"))
}

/// Text after a reasoning model's `<think>` block; none while it thinks.
fn after_thinking(text: &str) -> Option<&str> {
    match text.find("</think>") {
        Some(end) => text.get(end + "</think>".len()..),
        None if text.trim_start().starts_with("<think>") => None,
        None => Some(text),
    }
}

fn strip_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.split_once('\n').map_or(rest, |(_, body)| body);
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
}

/// Where a prefix stands: open brackets and whether a string is open.
#[derive(Clone, Debug, Default)]
struct Open {
    brackets: Vec<char>,
    in_string: bool,
    /// The prefix ends inside an escape, so its last character goes.
    escape: bool,
}

struct Scan {
    /// The state at the end, unless a closer didn't match.
    end: Option<Open>,
    /// Commas outside strings, with the state just before each.
    cuts: Vec<(usize, Open)>,
}

impl Scan {
    fn of(body: &str) -> Self {
        let mut open = Open::default();
        let mut cuts = Vec::new();
        for (i, c) in body.char_indices() {
            if open.in_string {
                match (open.escape, c) {
                    (true, _) => open.escape = false,
                    (false, '\\') => open.escape = true,
                    (false, '"') => open.in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => open.in_string = true,
                '{' | '[' => open.brackets.push(c),
                '}' | ']' => {
                    let want = if c == '}' { '{' } else { '[' };
                    if open.brackets.pop() != Some(want) {
                        return Self { end: None, cuts };
                    }
                }
                ',' => cuts.push((i, open.clone())),
                _ => {}
            }
        }
        Self {
            end: Some(open),
            cuts,
        }
    }
}

/// `prefix` closed as `open` says, if that parses.
fn close(prefix: &str, open: Option<&Open>) -> Option<Value> {
    let open = open?;
    let mut text = prefix.to_owned();
    if open.escape {
        text.pop();
    }
    if open.in_string {
        text.push('"');
    }
    for b in open.brackets.iter().rev() {
        text.push(if *b == '{' { '}' } else { ']' });
    }
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn whole_answers_come_bare_fenced_or_wrapped() {
        let want = json!({"a": 1});
        assert_eq!(extract(r#"{"a":1}"#), Some(want.clone()));
        assert_eq!(extract("```json\n{\"a\":1}\n```"), Some(want.clone()));
        assert_eq!(
            extract("Sure! {\"a\":1} Hope it helps."),
            Some(want.clone())
        );
        assert_eq!(extract("<think>{maybe}</think>\n{\"a\":1}"), Some(want));
        assert_eq!(extract("[1, 2]"), Some(json!([1, 2])));
        assert_eq!(extract("no json here"), None);
        assert_eq!(extract("} backwards {"), None);
    }

    #[test]
    fn a_prefix_shows_what_it_has() {
        let p = |s: &str| partial(s).map(|v| v.to_string());
        assert_eq!(
            p(r#"{"pos":"adj","meaning":"Moving fa"#).as_deref(),
            Some(r#"{"pos":"adj","meaning":"Moving fa"}"#)
        );
        assert_eq!(
            p(r#"{"pos":"adj","mea"#).as_deref(),
            Some(r#"{"pos":"adj"}"#)
        );
        assert_eq!(
            p(r#"{"pos":"adj","meaning":"#).as_deref(),
            Some(r#"{"pos":"adj"}"#)
        );
        assert_eq!(p(r#"{"s":["a","b"#).as_deref(), Some(r#"{"s":["a","b"]}"#));
        assert_eq!(p(r#"{"m":"say \"#).as_deref(), Some(r#"{"m":"say "}"#));
        assert_eq!(p(r#"{"n":1"#).as_deref(), Some(r#"{"n":1}"#));
    }

    #[test]
    fn nothing_shows_before_an_object_or_while_thinking() {
        assert_eq!(partial(""), None);
        assert_eq!(partial("Sure"), None);
        assert_eq!(partial("<think>{\"a\":"), None);
        assert_eq!(partial("{\"a\":1}]"), None, "a stray closer");
        assert_eq!(partial("{").map(|v| v.to_string()).as_deref(), Some("{}"));
    }

    fn word() -> Value {
        json!({
            "type": "object",
            "properties": {
                "meaning": { "type": "string", "description": "What it means." },
                "level": { "type": "string", "enum": ["easy", "hard"] },
                "count": { "type": "integer" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "note": { "anyOf": [{ "type": "string" }, { "type": "null" }] }
            },
            "required": ["meaning", "tags"]
        })
    }

    #[test]
    fn checks_name_what_is_wrong() {
        let s = word();
        assert!(check(&json!({"meaning": "m", "tags": [], "extra": 1}), &s).is_ok());
        assert!(check(&json!({"meaning": "m", "tags": ["a"], "count": 2.0}), &s).is_ok());
        let e = check(&json!({"definition": "m"}), &s).unwrap_err();
        assert_eq!(e, "$ lacks meaning, tags; it has definition");
        let e = check(&json!({"meaning": "m", "tags": "a, b"}), &s).unwrap_err();
        assert_eq!(e, "$.tags is a string, not the expected type");
        let e = check(&json!({"meaning": "m", "tags": [1]}), &s).unwrap_err();
        assert_eq!(e, "$.tags[0] is a number, not the expected type");
        let e = check(&json!({"meaning": "m", "tags": [], "level": "mid"}), &s).unwrap_err();
        assert_eq!(e, "$.level is not one of the allowed values");
        let e = check(&json!({"meaning": "m", "tags": [], "note": 3}), &s).unwrap_err();
        assert_eq!(e, "$.note matches none of the choices");
        assert!(check(&json!("anything"), &json!(true)).is_ok());
    }

    #[test]
    fn a_skeleton_shows_every_key() {
        let want = json!({
            "meaning": "<What it means.>",
            "level": "easy",
            "count": 0,
            "tags": ["..."],
            "note": null
        });
        assert_eq!(skeleton(&word()), want);
    }
}
