use anyhow::{Context, anyhow};
use bytes::Bytes;
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{config::PaperlessConfig, signature::SignatureInfo};

#[derive(Debug, Clone)]
pub struct PaperlessClient {
    client: Client,
    config: PaperlessConfig,
}

#[derive(Debug, Serialize)]
pub struct PaperlessUpdateResult {
    pub document_id: i64,
    pub tag_id: i64,
    pub tag_name: String,
    pub tag_created: bool,
    pub tag_applied: bool,
    pub note_added: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TagSummary {
    pub id: i64,
    pub name: String,
}

#[derive(Debug)]
struct Auth {
    header_value: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
struct Paginated<T> {
    results: Vec<T>,
    next: Option<String>,
}

impl PaperlessClient {
    pub fn new(config: PaperlessConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    pub async fn update_signed_document(
        &self,
        document_id: i64,
        signatures: &[SignatureInfo],
    ) -> anyhow::Result<PaperlessUpdateResult> {
        let auth = self.auth().await?;
        let (tag_id, tag_created) = self.ensure_tag(&auth).await?;
        self.add_tag(&auth, document_id, tag_id).await?;
        self.add_note(&auth, document_id, signatures).await?;

        Ok(PaperlessUpdateResult {
            document_id,
            tag_id,
            tag_name: self.config.tag_name.clone(),
            tag_created,
            tag_applied: true,
            note_added: true,
        })
    }

    pub async fn download_document(&self, document_id: i64) -> anyhow::Result<Bytes> {
        let auth = self.auth().await?;
        let url = self.url(&format!(
            "/api/documents/{document_id}/download/?original=true"
        ));
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, auth.header_value)
            .header(ACCEPT, self.accept_header())
            .send()
            .await
            .context("failed to download document from Paperless")?;

        ensure_success(response)
            .await?
            .bytes()
            .await
            .map_err(Into::into)
    }

    pub async fn list_documents_raw(&self) -> anyhow::Result<Vec<Value>> {
        let auth = self.auth().await?;
        let mut next = Some(self.url("/api/documents/?page_size=100&ordering=id"));
        let mut documents = Vec::new();

        while let Some(url) = next {
            let response = self
                .client
                .get(url)
                .header(AUTHORIZATION, &auth.header_value)
                .header(ACCEPT, self.accept_header())
                .send()
                .await
                .context("failed to list Paperless documents")?;

            let page = ensure_success(response)
                .await?
                .json::<Paginated<Value>>()
                .await
                .context("failed to parse Paperless documents response")?;

            documents.extend(page.results);
            next = page.next;
        }

        Ok(documents)
    }

    pub async fn list_tags(&self) -> anyhow::Result<Vec<TagSummary>> {
        let auth = self.auth().await?;
        let mut next = Some(self.url("/api/tags/?page_size=100"));
        let mut tags = Vec::new();

        while let Some(url) = next {
            let response = self
                .client
                .get(url)
                .header(AUTHORIZATION, &auth.header_value)
                .header(ACCEPT, self.accept_header())
                .send()
                .await
                .context("failed to list Paperless tags")?;

            let page = ensure_success(response)
                .await?
                .json::<Paginated<TagSummary>>()
                .await
                .context("failed to parse Paperless tags response")?;

            tags.extend(page.results);
            next = page.next;
        }

        Ok(tags)
    }

    async fn auth(&self) -> anyhow::Result<Auth> {
        if let Some(token) = &self.config.token {
            return Ok(Auth {
                header_value: format!("Token {token}"),
            });
        }

        let username = self
            .config
            .username
            .as_deref()
            .ok_or_else(|| anyhow!("PAPERLESS_USERNAME or PAPERLESS_TOKEN is required"))?;
        let password = self
            .config
            .password
            .as_deref()
            .ok_or_else(|| anyhow!("PAPERLESS_PASSWORD or PAPERLESS_TOKEN is required"))?;

        let token_url = self.url("/api/token/");
        let response = self
            .client
            .post(&token_url)
            .header(ACCEPT, "application/json")
            .json(&json!({
                "username": username,
                "password": password,
            }))
            .send()
            .await
            .with_context(|| format!("failed to request Paperless token from {token_url}"))?;

        let token = ensure_success(response)
            .await?
            .json::<TokenResponse>()
            .await
            .context("failed to parse Paperless token response")?
            .token;

        Ok(Auth {
            header_value: format!("Token {token}"),
        })
    }

    async fn ensure_tag(&self, auth: &Auth) -> anyhow::Result<(i64, bool)> {
        if let Some(tag) = self.find_tag(auth).await? {
            return Ok((tag.id, false));
        }

        let response = self
            .client
            .post(self.url("/api/tags/"))
            .header(AUTHORIZATION, &auth.header_value)
            .header(ACCEPT, self.accept_header())
            .json(&json!({
                "name": self.config.tag_name,
            }))
            .send()
            .await
            .context("failed to create Paperless tag")?;

        if response.status() == StatusCode::BAD_REQUEST {
            if let Some(tag) = self.find_tag(auth).await? {
                return Ok((tag.id, false));
            }
        }

        let tag = ensure_success(response)
            .await?
            .json::<TagSummary>()
            .await
            .context("failed to parse created Paperless tag")?;
        Ok((tag.id, true))
    }

    async fn find_tag(&self, auth: &Auth) -> anyhow::Result<Option<TagSummary>> {
        let tags = self.list_tags_with_auth(auth).await?;

        Ok(tags
            .into_iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(&self.config.tag_name)))
    }

    async fn list_tags_with_auth(&self, auth: &Auth) -> anyhow::Result<Vec<TagSummary>> {
        let mut next = Some(self.url("/api/tags/?page_size=100"));
        let mut tags = Vec::new();

        while let Some(url) = next {
            let response = self
                .client
                .get(url)
                .header(AUTHORIZATION, &auth.header_value)
                .header(ACCEPT, self.accept_header())
                .send()
                .await
                .context("failed to list Paperless tags")?;

            let page = ensure_success(response)
                .await?
                .json::<Paginated<TagSummary>>()
                .await
                .context("failed to parse Paperless tags response")?;

            tags.extend(page.results);
            next = page.next;
        }

        Ok(tags)
    }

    async fn add_tag(&self, auth: &Auth, document_id: i64, tag_id: i64) -> anyhow::Result<()> {
        let response = self
            .client
            .post(self.url("/api/documents/bulk_edit/"))
            .header(AUTHORIZATION, &auth.header_value)
            .header(ACCEPT, self.accept_header())
            .json(&json!({
                "documents": [document_id],
                "method": "add_tag",
                "parameters": {
                    "tag": tag_id,
                },
            }))
            .send()
            .await
            .context("failed to apply Paperless tag")?;

        let _ = ensure_success(response).await?.json::<Value>().await.ok();
        Ok(())
    }

    async fn add_note(
        &self,
        auth: &Auth,
        document_id: i64,
        signatures: &[SignatureInfo],
    ) -> anyhow::Result<()> {
        let response = self
            .client
            .post(self.url(&format!("/api/documents/{document_id}/notes/")))
            .header(AUTHORIZATION, &auth.header_value)
            .header(ACCEPT, self.accept_header())
            .json(&json!({
                "note": crate::signature::signatures_note(signatures),
            }))
            .send()
            .await
            .context("failed to add Paperless document note")?;

        let _ = ensure_success(response).await?.json::<Value>().await.ok();
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.url, path)
    }

    fn accept_header(&self) -> String {
        format!("application/json; version={}", self.config.api_version)
    }
}

async fn ensure_success(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!("Paperless API returned {status}: {body}"))
}
