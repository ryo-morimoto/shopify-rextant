use anyhow::Result;
use regex::Regex;
use url::Url;

pub(crate) fn is_indexable_shopify_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|url| {
            let host_ok = matches!(url.host_str(), Some("shopify.dev" | "www.shopify.dev"));
            let path = url.path();
            Some(host_ok && (path.starts_with("/docs/") || path.starts_with("/changelog")))
        })
        .unwrap_or(false)
}

pub(crate) fn raw_doc_candidates(url: &str) -> Result<Vec<String>> {
    let parsed = Url::parse(url)?;
    let mut base = parsed;
    base.set_query(None);
    base.set_fragment(None);
    let clean = base.to_string().trim_end_matches('/').to_string();
    let mut candidates = Vec::new();
    if clean.ends_with(".md") || clean.ends_with(".txt") {
        candidates.push(clean);
    } else {
        candidates.push(format!("{clean}.md"));
        candidates.push(format!("{clean}.txt"));
    }
    Ok(candidates)
}

pub(crate) fn canonical_doc_path(url: &str) -> Result<String> {
    let parsed = Url::parse(url)?;
    let mut path = parsed.path().trim_end_matches('/').to_string();
    if path.starts_with("/docs/") || path.starts_with("/changelog") {
        if let Some(stripped) = path.strip_suffix(".md") {
            path = stripped.to_string();
        }
        if let Some(stripped) = path.strip_suffix(".txt") {
            path = stripped.to_string();
        }
    }
    if path.is_empty() {
        path = "/".to_string();
    }
    Ok(path)
}

pub(crate) fn raw_path_for(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let mut safe = String::new();
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.') {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        "index.md".to_string()
    } else {
        format!("{safe}.md")
    }
}

pub(crate) fn extract_version(path: &str) -> Option<String> {
    let re = Regex::new(r"20\d{2}-\d{2}|latest").expect("valid regex");
    re.find(path).map(|m| m.as_str().to_string())
}

pub(crate) fn classify_doc_type(path: &str) -> String {
    if path == "/docs/api/admin-graphql"
        || path == "/docs/api/storefront"
        || path.contains("/reference")
        || path.contains("/objects/")
        || path.contains("/queries/")
        || path.contains("/mutations/")
        || path.starts_with("/docs/api/")
    {
        "reference"
    } else if path.contains("/tutorial") || path.contains("/build/") {
        "tutorial"
    } else if path.contains("/migrate") || path.contains("/migration") {
        "migration"
    } else if path.contains("/guide") || path.contains("/how-to") {
        "how-to"
    } else {
        "explanation"
    }
    .to_string()
}

pub(crate) fn classify_content_class(path: &str) -> String {
    if path == "/docs/api/admin-graphql" || path == "/docs/api/storefront" {
        "api_ref"
    } else if path.contains("/admin-graphql/") || path.contains("/storefront/") {
        "schema_ref"
    } else if path.contains("/liquid/") {
        "liquid_ref"
    } else if path.contains("/changelog") {
        "changelog"
    } else if path.contains("/api/") {
        "api_ref"
    } else if path.contains("/tutorial") {
        "tutorial"
    } else {
        "guide"
    }
    .to_string()
}

pub(crate) fn classify_api_surface(path: &str) -> Option<String> {
    let surface = if path == "/docs/api/admin-graphql" || path.contains("/admin-graphql/") {
        "admin_graphql"
    } else if path == "/docs/api/storefront" || path.contains("/storefront/") {
        "storefront"
    } else if path.contains("/liquid/") {
        "liquid"
    } else if path.contains("/hydrogen/") {
        "hydrogen"
    } else if path.contains("/functions/") {
        "functions"
    } else if path.contains("/polaris") {
        "polaris"
    } else if path.contains("/flow/") {
        "flow"
    } else {
        return None;
    };
    Some(surface.to_string())
}

pub(crate) fn reading_time_min(content: &str) -> i64 {
    let words = content.split_whitespace().count() as i64;
    (words / 220).max(1)
}
