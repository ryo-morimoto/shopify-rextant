use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;

use crate::url_policy::{canonical_doc_path, is_indexable_shopify_url};

#[derive(Debug)]
pub(crate) struct MarkdownLink {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) source: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SectionInfo {
    pub(crate) anchor: String,
    pub(crate) title: String,
    pub(crate) level: usize,
    pub(crate) char_range: [usize; 2],
}

pub(crate) fn parse_markdown_links(markdown: &str) -> Vec<MarkdownLink> {
    let re = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").expect("valid regex");
    re.captures_iter(markdown)
        .filter_map(|caps| {
            let title = caps.get(1)?.as_str().trim().to_string();
            let raw_url = caps.get(2)?.as_str().trim();
            let url = if raw_url.starts_with("http") {
                raw_url.to_string()
            } else if raw_url.starts_with('/') {
                format!("https://shopify.dev{raw_url}")
            } else {
                return None;
            };
            Some(MarkdownLink {
                title,
                url,
                source: "llms".to_string(),
            })
        })
        .collect()
}

pub(crate) fn parse_sitemap_links(xml: &str) -> Vec<MarkdownLink> {
    let re = Regex::new(r"(?s)<loc>\s*([^<]+?)\s*</loc>").expect("valid regex");
    re.captures_iter(xml)
        .filter_map(|caps| {
            let url = caps.get(1)?.as_str().trim().to_string();
            if !is_indexable_shopify_url(&url) {
                return None;
            }
            let title = canonical_doc_path(&url).ok()?;
            Some(MarkdownLink {
                title,
                url,
                source: "sitemap".to_string(),
            })
        })
        .collect()
}

pub(crate) fn dedupe_links_by_path(links: Vec<MarkdownLink>) -> Vec<MarkdownLink> {
    let mut seen = HashSet::new();
    links
        .into_iter()
        .filter(|link| {
            canonical_doc_path(&link.url)
                .map(|path| seen.insert(path))
                .unwrap_or(false)
        })
        .collect()
}

pub(crate) fn extract_sections(markdown: &str) -> Vec<SectionInfo> {
    let mut headings = Vec::new();
    let mut offset = 0;
    for line in markdown.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if let Some((level, title)) = parse_heading(line_without_newline) {
            headings.push((offset, level, title.to_string()));
        }
        offset += line.len();
    }

    headings
        .iter()
        .enumerate()
        .map(|(index, (start, level, title))| {
            let end = headings
                .iter()
                .skip(index + 1)
                .find(|(_, next_level, _)| next_level <= level)
                .map(|(next_start, _, _)| *next_start)
                .unwrap_or(markdown.len());
            SectionInfo {
                anchor: slugify_heading(title),
                title: title.clone(),
                level: *level,
                char_range: [*start, end],
            }
        })
        .collect()
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let title = line.get(hashes..)?.strip_prefix(' ')?.trim();
    if title.is_empty() {
        None
    } else {
        Some((hashes, title))
    }
}

fn slugify_heading(title: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if ch.is_whitespace() || matches!(ch, '-' | '_' | '/') {
            if !slug.is_empty() && !last_dash {
                slug.push('-');
                last_dash = true;
            }
        }
    }
    slug.trim_matches('-').to_string()
}

pub(crate) fn section_content(
    markdown: &str,
    sections: &[SectionInfo],
    anchor: &str,
) -> Option<String> {
    let normalized = anchor.trim_start_matches('#');
    sections
        .iter()
        .find(|section| section.anchor == normalized)
        .and_then(|section| markdown.get(section.char_range[0]..section.char_range[1]))
        .map(ToOwned::to_owned)
}

pub(crate) fn remove_fenced_code_blocks(markdown: &str) -> String {
    let mut output = String::new();
    let mut in_fence = false;
    let mut fence_marker = "";
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !in_fence && trimmed.starts_with("```") {
            in_fence = true;
            fence_marker = "```";
            continue;
        }
        if !in_fence && trimmed.starts_with("~~~") {
            in_fence = true;
            fence_marker = "~~~";
            continue;
        }
        if in_fence && trimmed.starts_with(fence_marker) {
            in_fence = false;
            fence_marker = "";
            continue;
        }
        if !in_fence {
            output.push_str(line);
        }
    }
    output
}

pub(crate) fn title_from_markdown(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        line.strip_prefix("# ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    })
}
