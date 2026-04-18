use regex::Regex;
use std::collections::BTreeSet;

use super::super::url_policy::canonical_doc_path;
use super::super::util::hash::hex_sha256;
use super::types::{ChangelogEntryInput, ResolvedImpact, ScheduledChangeRecord};

pub(crate) fn extract_impact_candidates(entry: &ChangelogEntryInput) -> Vec<String> {
    let mut candidates = BTreeSet::new();
    let text = format!(
        "{}\n{}\n{}\n{}",
        entry.title,
        entry.body,
        entry.link,
        entry.categories.join("\n")
    );
    let doc_re = Regex::new(
        r#"https://shopify\.dev/(?:docs|changelog)/[^\s<>)"']+|/(?:docs|changelog)/[^\s<>)"']+"#,
    )
    .expect("valid changelog doc path regex");
    for caps in doc_re.captures_iter(&text) {
        candidates.insert(trim_candidate(caps.get(0).unwrap().as_str()));
    }
    let version_re = Regex::new(r"\b20\d{2}-\d{2}\b").expect("valid API version regex");
    for caps in version_re.captures_iter(&text) {
        candidates.insert(caps.get(0).unwrap().as_str().to_string());
    }
    let symbol_re = Regex::new(r"\b[A-Z][A-Za-z0-9]+(?:\.[A-Za-z_][A-Za-z0-9_]*)?\b")
        .expect("valid GraphQL symbol regex");
    for caps in symbol_re.captures_iter(&text) {
        candidates.insert(trim_candidate(caps.get(0).unwrap().as_str()));
    }
    candidates.into_iter().collect()
}

pub(crate) fn trim_candidate(value: &str) -> String {
    value
        .trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | ')' | '(' | '"' | '\''))
        .to_string()
}

pub(crate) fn is_api_version(value: &str) -> bool {
    Regex::new(r"^20\d{2}-\d{2}$")
        .expect("valid API version regex")
        .is_match(value)
}

pub(crate) fn candidate_to_doc_path(candidate: &str) -> Option<String> {
    if candidate.starts_with("/docs/") || candidate.starts_with("/changelog") {
        return Some(candidate.trim_end_matches('/').to_string());
    }
    if candidate.starts_with("https://shopify.dev/")
        || candidate.starts_with("https://www.shopify.dev/")
    {
        return canonical_doc_path(candidate).ok();
    }
    None
}

pub(crate) fn looks_like_reference_candidate(candidate: &str) -> bool {
    candidate.contains('.') || candidate.chars().next().is_some_and(char::is_uppercase)
}

pub(crate) fn surface_from_category(category: &str) -> Option<String> {
    let normalized = category.to_ascii_lowercase();
    if normalized.contains("admin graphql") || normalized.contains("graphql admin") {
        Some("admin_graphql".to_string())
    } else if normalized.contains("storefront") {
        Some("storefront".to_string())
    } else if normalized.contains("liquid") {
        Some("liquid".to_string())
    } else if normalized.contains("polaris") {
        Some("polaris".to_string())
    } else {
        None
    }
}

pub(crate) fn scheduled_changes_from_entry(
    entry: &ChangelogEntryInput,
    impact: &ResolvedImpact,
) -> Vec<ScheduledChangeRecord> {
    let change = classify_change(entry);
    if change.is_none() || impact.refs.is_empty() {
        return Vec::new();
    }
    let change = change.unwrap();
    let effective_date = extract_effective_date(entry);
    let migration_hint = extract_migration_hint(entry);
    impact
        .refs
        .iter()
        .map(|reference| {
            let id = hex_sha256(&format!(
                "{}:{}:{}:{}",
                entry.id,
                reference,
                change,
                effective_date.as_deref().unwrap_or("")
            ));
            ScheduledChangeRecord {
                id,
                type_name: reference.clone(),
                change: change.clone(),
                effective_date: effective_date.clone(),
                migration_hint: migration_hint.clone(),
                source_changelog_id: entry.id.clone(),
            }
        })
        .collect()
}

pub(crate) fn classify_change(entry: &ChangelogEntryInput) -> Option<String> {
    let text = format!("{}\n{}", entry.title, entry.body).to_ascii_lowercase();
    if text.contains("removed") || text.contains("removal") || text.contains("will be removed") {
        Some("removal".to_string())
    } else if text.contains("deprecated") || text.contains("deprecation") {
        Some("deprecation".to_string())
    } else {
        None
    }
}

pub(crate) fn extract_effective_date(entry: &ChangelogEntryInput) -> Option<String> {
    let text = format!("{}\n{}", entry.title, entry.body);
    Regex::new(r"\b20\d{2}-\d{2}\b")
        .expect("valid API version regex")
        .find(&text)
        .map(|m| m.as_str().to_string())
        .or_else(|| {
            Regex::new(r"\b20\d{2}-\d{2}-\d{2}\b")
                .expect("valid date regex")
                .find(&text)
                .map(|m| m.as_str().to_string())
        })
}

pub(crate) fn extract_migration_hint(entry: &ChangelogEntryInput) -> Option<String> {
    entry
        .body
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("migrat"))
        .map(|line| line.trim().chars().take(300).collect::<String>())
}

pub(crate) fn impact_affected_types(impact: &ResolvedImpact) -> Vec<String> {
    let mut affected = impact
        .refs
        .iter()
        .chain(impact.concept_ids.iter())
        .cloned()
        .collect::<Vec<_>>();
    affected.sort();
    affected.dedup();
    affected
}
