use anyhow::{Context, Result};
use chrono::Utc;
use feed_rs::parser;
use regex::Regex;
use std::collections::BTreeSet;

use super::types::ChangelogEntryInput;

pub(crate) fn parse_changelog_feed(xml: &str) -> Result<Vec<ChangelogEntryInput>> {
    let feed = parser::parse(xml.as_bytes()).context("parse RSS/Atom changelog feed")?;
    Ok(feed
        .entries
        .into_iter()
        .map(|entry| {
            let link = entry
                .links
                .first()
                .map(|link| link.href.clone())
                .unwrap_or_else(|| entry.id.clone());
            let id = if entry.id.trim().is_empty() {
                link.clone()
            } else {
                entry.id
            };
            let title = entry
                .title
                .map(|title| title.content)
                .unwrap_or_else(|| id.clone());
            let body = entry
                .content
                .and_then(|content| content.body)
                .or_else(|| entry.summary.map(|summary| summary.content))
                .unwrap_or_default();
            let posted_at = entry
                .published
                .or(entry.updated)
                .unwrap_or_else(Utc::now)
                .to_rfc3339();
            let categories = entry
                .categories
                .into_iter()
                .flat_map(|category| [Some(category.term), category.label].into_iter().flatten())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            ChangelogEntryInput {
                id,
                title,
                link,
                body,
                posted_at,
                categories,
            }
        })
        .collect())
}

pub(crate) fn version_candidates_desc(page: &str) -> Vec<String> {
    let re = Regex::new(r"\b20\d{2}-\d{2}\b").expect("valid API version regex");
    re.find_iter(page)
        .map(|m| m.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .rev()
        .collect()
}
