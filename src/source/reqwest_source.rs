use anyhow::Result;
use reqwest::StatusCode;
use serde_json::json;

use super::super::SourceFetchError;
use super::super::{ADMIN_GRAPHQL_INTROSPECTION_QUERY, USER_AGENT};
use super::text_source::TextSource;

pub(crate) struct ReqwestTextSource {
    client: reqwest::Client,
}

impl ReqwestTextSource {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().user_agent(USER_AGENT).build()?,
        })
    }
}

impl TextSource for ReqwestTextSource {
    async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceFetchError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: None,
            })?;
        let status = response.status();
        if status == StatusCode::OK {
            return response.text().await.map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: Some(StatusCode::OK.as_u16()),
            });
        }
        Err(SourceFetchError {
            status: "skipped".to_string(),
            reason: format!("http_status: {status}"),
            http_status: Some(status.as_u16()),
        })
    }

    async fn fetch_admin_graphql_introspection(
        &self,
        url: &str,
    ) -> std::result::Result<String, SourceFetchError> {
        let response = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&json!({
                    "query": ADMIN_GRAPHQL_INTROSPECTION_QUERY
                }))
                .map_err(|e| SourceFetchError {
                    status: "failed".to_string(),
                    reason: format!("serialize_introspection_query: {e}"),
                    http_status: None,
                })?,
            )
            .send()
            .await
            .map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: None,
            })?;
        let status = response.status();
        if status == StatusCode::OK {
            return response.text().await.map_err(|e| SourceFetchError {
                status: "failed".to_string(),
                reason: format!("network_error: {e}"),
                http_status: Some(StatusCode::OK.as_u16()),
            });
        }
        Err(SourceFetchError {
            status: "skipped".to_string(),
            reason: format!("http_status: {status}"),
            http_status: Some(status.as_u16()),
        })
    }
}
