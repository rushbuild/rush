// Rush HTTP Remote Cache
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! HTTP-based remote cache for action results.
//!
//! Compatible with Bazel's HTTP cache protocol.

use anyhow::Result;
use rush_action::ActionId;
use rush_exec::ActionResult;
use std::time::Duration;

/// HTTP remote cache client.
#[derive(Debug, Clone)]
pub struct HttpCache {
    /// Base URL for the cache server.
    base_url: String,

    /// HTTP client.
    client: reqwest::Client,
}

impl HttpCache {
    /// Create a new HTTP cache client.
    pub fn new(base_url: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        Ok(HttpCache {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// Get action result from remote cache.
    pub async fn get(&self, action_id: &ActionId) -> Result<Option<ActionResult>> {
        let key = action_id.to_string();
        let url = format!("{}/ac/{}", self.base_url, key);

        match self.client.get(&url).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    let bytes = response.bytes().await?;
                    let result: ActionResult = bincode::deserialize(&bytes)?;
                    Ok(Some(result))
                } else if response.status().as_u16() == 404 {
                    Ok(None)
                } else {
                    anyhow::bail!("HTTP cache GET failed: {}", response.status())
                }
            }
            Err(e) => {
                // Log but don't fail on network errors
                eprintln!("Warning: HTTP cache GET failed: {}", e);
                Ok(None)
            }
        }
    }

    /// Put action result to remote cache.
    pub async fn put(&self, action_id: &ActionId, result: &ActionResult) -> Result<()> {
        let key = action_id.to_string();
        let url = format!("{}/ac/{}", self.base_url, key);

        let data = bincode::serialize(result)?;

        match self.client.put(&url).body(data).send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    eprintln!("Warning: HTTP cache PUT failed: {}", response.status());
                }
                Ok(())
            }
            Err(e) => {
                // Log but don't fail on network errors
                eprintln!("Warning: HTTP cache PUT failed: {}", e);
                Ok(())
            }
        }
    }

    /// Check if cache is reachable.
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url);

        match self.client.get(&url).send().await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    /// Get base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_cache_creation() {
        let cache = HttpCache::new("http://localhost:8080".to_string());
        assert!(cache.is_ok());

        let cache = cache.unwrap();
        assert_eq!(cache.base_url(), "http://localhost:8080");
    }

    #[test]
    fn test_url_normalization() {
        let cache = HttpCache::new("http://localhost:8080/".to_string()).unwrap();
        assert_eq!(cache.base_url(), "http://localhost:8080");
    }
}
