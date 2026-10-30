//! Minimal Google Drive API v3 client.
use crate::auth::{self, Token};
use crate::config::Config;
use crate::now;
use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
pub const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
pub const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

const API: &str = "https://www.googleapis.com/drive/v3";
const FILE_FIELDS: &str = "id,name,mimeType,size,md5Checksum,modifiedTime,parents,trashed,webViewLink";

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    /// int64 encoded as a string by the API.
    pub size: Option<String>,
    pub md5_checksum: Option<String>,
    /// RFC 3339.
    pub modified_time: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub trashed: bool,
    pub web_view_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    next_page_token: Option<String>,
    #[serde(default)]
    files: Vec<RemoteFile>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    pub file: Option<RemoteFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangeList {
    next_page_token: Option<String>,
    new_start_page_token: Option<String>,
    #[serde(default)]
    changes: Vec<Change>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartPageToken {
    start_page_token: String,
}

pub struct Drive {
    http: reqwest::Client,
    cfg: Config,
    token: Mutex<Token>,
}

impl Drive {
    pub fn new(cfg: Config) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()?,
            cfg,
            token: Mutex::new(Token::load()?),
        })
    }

    async fn bearer(&self) -> Result<String> {
        let mut t = self.token.lock().await;
        if t.expires_at - 60 < now() {
            *t = auth::refresh(&self.http, &self.cfg, &t).await?;
        }
        Ok(t.access_token.clone())
    }

    /// GET with auth, retrying on rate limits, server errors and transient network errors.
    async fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<reqwest::Response> {
        let mut delay = Duration::from_millis(500);
        for attempt in 0..6 {
            let last = attempt == 5;
            let res = self.http.get(url).bearer_auth(self.bearer().await?).query(query).send().await;
            match res {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) if r.status() == StatusCode::UNAUTHORIZED && !last => {
                    self.token.lock().await.expires_at = 0; // force refresh
                    continue;
                }
                Ok(r) if !last && (r.status() == StatusCode::TOO_MANY_REQUESTS || r.status().is_server_error()) => {}
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    let rate_limited = status == StatusCode::FORBIDDEN && body.contains("ateLimitExceeded");
                    if !(rate_limited && !last) {
                        bail!("Drive API {status}: {body}");
                    }
                }
                Err(e) if !last && (e.is_connect() || e.is_timeout()) => {}
                Err(e) => return Err(e.into()),
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        unreachable!()
    }

    pub async fn get_file(&self, id: &str) -> Result<RemoteFile> {
        let fields = FILE_FIELDS;
        Ok(self.get(&format!("{API}/files/{id}"), &[("fields", fields)]).await?.json().await?)
    }

    async fn list(&self, q: &str, mut on_page: impl FnMut(Vec<RemoteFile>) -> Result<()>) -> Result<()> {
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut page: Option<String> = None;
        loop {
            let mut query = vec![
                ("q", q),
                ("fields", fields.as_str()),
                ("pageSize", "1000"),
                ("spaces", "drive"),
            ];
            if let Some(p) = &page {
                query.push(("pageToken", p.as_str()));
            }
            let list: FileList = self.get(&format!("{API}/files"), &query).await?.json().await?;
            on_page(list.files)?;
            match list.next_page_token {
                Some(p) => page = Some(p),
                None => return Ok(()),
            }
        }
    }

    /// Every non-trashed file the user can see, page by page.
    pub async fn list_all(&self, on_page: impl FnMut(Vec<RemoteFile>) -> Result<()>) -> Result<()> {
        self.list("trashed = false", on_page).await
    }

    pub async fn list_children(&self, parent_id: &str) -> Result<Vec<RemoteFile>> {
        let mut out = Vec::new();
        let q = format!("'{parent_id}' in parents and trashed = false");
        self.list(&q, |files| {
            out.extend(files);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    pub async fn start_page_token(&self) -> Result<String> {
        let r: StartPageToken = self.get(&format!("{API}/changes/startPageToken"), &[]).await?.json().await?;
        Ok(r.start_page_token)
    }

    /// All changes since `token`. Returns the changes and the token for next time.
    pub async fn changes(&self, token: &str) -> Result<(Vec<Change>, String)> {
        let fields = format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))");
        let mut page = token.to_string();
        let mut all = Vec::new();
        loop {
            let list: ChangeList = self
                .get(
                    &format!("{API}/changes"),
                    &[
                        ("pageToken", page.as_str()),
                        ("fields", fields.as_str()),
                        ("pageSize", "1000"),
                        ("includeRemoved", "true"),
                        ("restrictToMyDrive", "true"),
                        ("spaces", "drive"),
                    ],
                )
                .await?
                .json()
                .await?;
            all.extend(list.changes);
            if let Some(next) = list.new_start_page_token {
                return Ok((all, next));
            }
            page = list.next_page_token.context("respuesta de changes sin token")?;
        }
    }

    /// Downloads the content of a (non Google Docs) file to `dest`, atomically.
    pub async fn download(&self, id: &str, dest: &Path) -> Result<()> {
        let mut resp = self.get(&format!("{API}/files/{id}"), &[("alt", "media")]).await?;
        let tmp = dest.with_extension("part");
        let mut f = tokio::fs::File::create(&tmp).await?;
        while let Some(chunk) = resp.chunk().await? {
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        drop(f);
        tokio::fs::rename(&tmp, dest).await?;
        Ok(())
    }
}
