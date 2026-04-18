use std::time::Duration;

use super::super::source::reqwest_source::ReqwestTextSource;
use super::super::{IndexSourceUrls, Paths, check_new_versions_from_source};

pub(crate) fn spawn_background_workers(paths: Paths) {
    tokio::spawn(async move {
        let source_urls = IndexSourceUrls::default();
        let source = match ReqwestTextSource::new() {
            Ok(source) => source,
            Err(error) => {
                eprintln!("version_watcher setup error: {error}");
                return;
            }
        };
        let mut interval = tokio::time::interval(Duration::from_secs(86_400));
        loop {
            interval.tick().await;
            if let Err(error) = check_new_versions_from_source(&paths, &source_urls, &source).await
            {
                eprintln!("version_watcher error: {error}");
            }
        }
    });
}
