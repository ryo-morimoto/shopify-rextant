use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;

pub(crate) fn to_json_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|e| json!({ "serialization_error": e.to_string() }))
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(crate) fn merge_json_arrays(mut left: Vec<Value>, right: Vec<Value>) -> Vec<Value> {
    let mut seen = left
        .iter()
        .filter_map(|value| serde_json::to_string(value).ok())
        .collect::<HashSet<_>>();
    for value in right {
        if serde_json::to_string(&value)
            .ok()
            .is_some_and(|key| seen.insert(key))
        {
            left.push(value);
        }
    }
    left
}

pub(crate) fn doc_json_field(doc_json: &str, field: &str) -> Option<String> {
    let value: Value = serde_json::from_str(doc_json).ok()?;
    value.get(field).and_then(|field_value| {
        field_value
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str)
            .or_else(|| field_value.as_str())
            .map(ToOwned::to_owned)
    })
}

pub(crate) fn escape_query(query: &str) -> String {
    query
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch.is_whitespace() {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
}
