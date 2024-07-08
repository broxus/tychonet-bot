use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::{header, Url};
use serde::Deserialize;

const USER_AGENT: &str = "tychonet-bot/1.0";

#[derive(Clone)]
#[repr(transparent)]
pub struct GithubClient {
    inner: Arc<Inner>,
}

impl GithubClient {
    pub fn new(token: &str, org: &str, repo: &str) -> Result<Self> {
        let base_url = format!("https://api.github.com/repos/{org}/{repo}/").parse()?;

        let mut headers = reqwest::header::HeaderMap::new();

        let mut bearer_header = reqwest::header::HeaderValue::try_from(format!("Bearer {token}"))?;
        bearer_header.set_sensitive(true);

        headers.insert(reqwest::header::AUTHORIZATION, bearer_header);
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(USER_AGENT),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            reqwest::header::HeaderValue::from_static("2022-11-28"),
        );

        let client = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .build()
            .context("failed to build github client")?;

        Ok(Self {
            inner: Arc::new(Inner { client, base_url }),
        })
    }

    pub async fn get_commit_sha(&self, branch: &str) -> Result<String> {
        let this = &self.inner;

        let url = this.base_url.join(&format!("commits/{branch}"))?;
        let response = this
            .client
            .get(url)
            .header(header::ACCEPT, "application/vnd.github.sha")
            .send()
            .await?
            .error_for_status()?;

        response.text().await.context("failed to get commit sha")
    }

    pub async fn get_commit_info(&self, commit_sha: &str) -> Result<CommitInfo> {
        let this = &self.inner;

        let url = this.base_url.join(&format!("git/commits/{commit_sha}"))?;
        let response = this
            .client
            .get(url)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;

        response.json().await.context("failed to get commit info")
    }

    pub async fn get_commit_branches(&self, commit_sha: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct BranchInfo {
            name: String,
        }

        let this = &self.inner;

        let url = this
            .base_url
            .join(&format!("commits/{commit_sha}/branches-where-head"))?;
        let response = this
            .client
            .get(url)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;

        response
            .json::<Vec<BranchInfo>>()
            .await
            .map(|res| res.into_iter().map(|info| info.name).collect())
            .context("failed to get commit info")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitInfo {
    pub html_url: String,
    pub message: String,
}

struct Inner {
    client: reqwest::Client,
    base_url: Url,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ignore]
    #[tokio::test]
    async fn test_client() -> Result<()> {
        tracing_subscriber::fmt::init();

        const TOKEN: &str = "";

        let client = GithubClient::new(TOKEN, "broxus", "tycho")?;

        let sha = client.get_commit_sha("master").await?;
        println!("SHA: {sha}");

        let info = client.get_commit_info(&sha).await?;
        println!("Info: {info:?}");

        let branches = client.get_commit_branches(&sha).await?;
        println!("Branches: {branches:?}");

        Ok(())
    }
}
