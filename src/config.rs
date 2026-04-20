use std::fs;

use anyhow::{Result, bail};

use super::Paths;
use super::ToolError;
use super::on_demand::{
    FetchCandidate as OnDemandFetchCandidate, FetchPolicy as OnDemandFetchPolicy,
};

#[derive(Debug, Clone)]
pub(crate) struct AppConfig {
    pub(crate) index: IndexConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexConfig {
    pub(crate) enable_on_demand_fetch: bool,
}

pub(crate) fn load_config(paths: &Paths) -> Result<AppConfig> {
    let raw = match fs::read_to_string(paths.config_file()) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut section = String::new();
    let mut enable_on_demand_fetch = false;
    for line in raw.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']']).trim().to_string();
            continue;
        }
        if section == "index" {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "enable_on_demand_fetch" {
                enable_on_demand_fetch = match value.trim() {
                    "true" => true,
                    "false" => false,
                    other => bail!("invalid enable_on_demand_fetch value: {other}"),
                };
            }
        }
    }
    Ok(AppConfig {
        index: IndexConfig {
            enable_on_demand_fetch,
        },
    })
}

pub(crate) fn policy_from_config(config: &AppConfig) -> OnDemandFetchPolicy {
    OnDemandFetchPolicy::new(config.index.enable_on_demand_fetch)
}

pub(crate) fn ensure_on_demand_enabled(
    policy: &OnDemandFetchPolicy,
    candidate: &OnDemandFetchCandidate,
) -> std::result::Result<(), ToolError> {
    if policy.is_enabled() {
        Ok(())
    } else {
        Err(ToolError::disabled(candidate))
    }
}
