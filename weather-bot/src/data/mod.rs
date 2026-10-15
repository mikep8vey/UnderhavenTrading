pub mod cli_report;
pub mod ensemble;
pub mod metar;
pub mod station_registry;

use crate::error::{BotError, Result};
use crate::util::time::retry;

/// GET a URL as text with the standard timeout(5s) + retry(3, exponential backoff) wrapper.
pub async fn get_text(http: &reqwest::Client, url: &str, label: &str) -> Result<String> {
    let http = http.clone();
    let url = url.to_string();
    retry(label, move || {
        let http = http.clone();
        let url = url.clone();
        async move {
            let r = http.get(&url).send().await.map_err(|e| BotError::Network(e.to_string()))?;
            let status = r.status();
            if !status.is_success() {
                return Err(BotError::Network(format!("{url}: HTTP {status}")));
            }
            r.text().await.map_err(|e| BotError::Network(e.to_string()))
        }
    })
    .await
}
