use chrono::Utc;

pub(crate) fn now_iso() -> String {
    Utc::now().to_rfc3339()
}
