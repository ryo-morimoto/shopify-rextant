use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;

use super::schema_urls::concept_id;

pub(crate) fn resolve_concept_id(
    version: &str,
    target_name: &str,
    concept_ids: &HashSet<String>,
) -> Option<String> {
    let exact = concept_id(version, target_name);
    if concept_ids.contains(&exact) {
        return Some(exact);
    }
    if let Some(stripped) = target_name.strip_suffix("Connection") {
        let stripped_id = concept_id(version, stripped);
        if concept_ids.contains(&stripped_id) {
            return Some(stripped_id);
        }
    }
    None
}

pub(crate) fn extract_named_type(value: &Value) -> Option<String> {
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        return Some(name.to_string());
    }
    value.get("ofType").and_then(extract_named_type)
}

pub(crate) fn markdown_mentions_type(markdown: &str, type_name: &str) -> bool {
    let pattern = format!(r"\b{}\b", regex::escape(type_name));
    Regex::new(&pattern)
        .expect("escaped type name regex is valid")
        .is_match(markdown)
}
