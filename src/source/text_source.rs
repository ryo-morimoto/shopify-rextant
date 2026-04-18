use super::super::SourceFetchError;

pub(crate) trait TextSource {
    async fn fetch_text(&self, url: &str) -> std::result::Result<String, SourceFetchError>;

    async fn fetch_admin_graphql_introspection(
        &self,
        url: &str,
    ) -> std::result::Result<String, SourceFetchError> {
        self.fetch_text(url).await
    }
}
