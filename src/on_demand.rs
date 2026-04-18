use url::Url;

#[derive(Debug, Clone)]
pub(crate) struct FetchPolicy {
    enabled: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FetchCandidate {
    pub(crate) canonical_path: String,
    pub(crate) source_url: String,
}

#[derive(Debug)]
pub(crate) enum PolicyError {
    OutsideScope,
}

impl FetchPolicy {
    pub(crate) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn candidate_from_input(
        input: &str,
    ) -> std::result::Result<FetchCandidate, PolicyError> {
        if input.starts_with('/') {
            return Self::candidate_from_path(input);
        }
        Self::candidate_from_url(input)
    }

    fn candidate_from_url(input: &str) -> std::result::Result<FetchCandidate, PolicyError> {
        let parsed = Url::parse(input).map_err(|_| PolicyError::OutsideScope)?;
        let host_ok = parsed.host_str() == Some("shopify.dev");
        if parsed.scheme() != "https" || !host_ok || !is_allowed_path(parsed.path()) {
            return Err(PolicyError::OutsideScope);
        }
        let canonical_path = normalize_path(parsed.path());
        Ok(FetchCandidate {
            source_url: format!("https://shopify.dev{canonical_path}"),
            canonical_path,
        })
    }

    fn candidate_from_path(input: &str) -> std::result::Result<FetchCandidate, PolicyError> {
        if !is_allowed_path(input) {
            return Err(PolicyError::OutsideScope);
        }
        let canonical_path = normalize_path(input);
        Ok(FetchCandidate {
            source_url: format!("https://shopify.dev{canonical_path}"),
            canonical_path,
        })
    }
}

pub(crate) fn normalize_path(path: &str) -> String {
    let mut path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim()
        .to_string();
    path = path.trim_end_matches('/').to_string();
    if let Some(stripped) = path.strip_suffix(".md") {
        path = stripped.to_string();
    }
    if let Some(stripped) = path.strip_suffix(".txt") {
        path = stripped.to_string();
    }
    path
}

pub(crate) fn is_allowed_path(path: &str) -> bool {
    path.starts_with("/docs/") || path.starts_with("/changelog/")
}
