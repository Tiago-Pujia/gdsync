//! Minimal Google Drive API v3 client.
use crate::auth::{self, Token};
use crate::config::Config;
use crate::now;
use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
pub const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
pub const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

const API: &str = "https://www.googleapis.com/drive/v3";
const FILE_FIELDS: &str = "id,name,mimeType,size,md5Checksum,modifiedTime,parents,trashed,webViewLink,shortcutDetails";

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutDetails {
    pub target_id: Option<String>,
    pub target_mime_type: Option<String>,
}

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
    pub shortcut_details: Option<ShortcutDetails>,
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

const UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageQuota {
    limit: Option<String>,
    usage: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AboutResponse {
    storage_quota: Option<StorageQuota>,
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
            if let Ok(on_disk) = Token::load() {
                if on_disk.refresh_token != t.refresh_token || on_disk.expires_at > t.expires_at {
                    *t = on_disk;
                }
            }
            if t.expires_at - 60 < now() {
                match auth::refresh(&self.http, &self.cfg, &t).await {
                    Ok(new_tok) => *t = new_tok,
                    Err(e) => {
                        if let Ok(on_disk) = Token::load() {
                            if on_disk.refresh_token != t.refresh_token && on_disk.expires_at - 60 >= now() {
                                *t = on_disk;
                                return Ok(t.access_token.clone());
                            }
                        }
                        return Err(e);
                    }
                }
            }
        }
        Ok(t.access_token.clone())
    }

    async fn request_bytes(
        &self,
        method: reqwest::Method,
        url: &str,
        query: &[(&str, &str)],
        body: Option<(Vec<u8>, String)>,
    ) -> Result<reqwest::Response> {
        let mut delay = Duration::from_millis(500);
        for attempt in 0..6 {
            let last = attempt == 5;
            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(self.bearer().await?)
                .query(query);

            if let Some((b, ct)) = &body {
                req = req.header(reqwest::header::CONTENT_TYPE, ct.as_str()).body(b.clone());
            }

            let res = req.send().await;
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

    async fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<reqwest::Response> {
        self.request_bytes(reqwest::Method::GET, url, query, None).await
    }

    async fn post_json(&self, url: &str, query: &[(&str, &str)], body: &serde_json::Value) -> Result<reqwest::Response> {
        let bytes = serde_json::to_vec(body)?;
        self.request_bytes(reqwest::Method::POST, url, query, Some((bytes, "application/json".into()))).await
    }

    async fn patch_json(&self, url: &str, query: &[(&str, &str)], body: &serde_json::Value) -> Result<reqwest::Response> {
        let bytes = serde_json::to_vec(body)?;
        self.request_bytes(reqwest::Method::PATCH, url, query, Some((bytes, "application/json".into()))).await
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

    /// List all non-trashed folders available in Drive.
    pub async fn list_all_folders(&self) -> Result<Vec<RemoteFile>> {
        let mut out = Vec::new();
        let q = format!("mimeType = '{FOLDER_MIME}' and trashed = false");
        self.list(&q, |files| {
            out.extend(files);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Search for folders matching an exact name or ID.
    pub async fn search_folder(&self, query: &str) -> Result<Vec<RemoteFile>> {
        if let Ok(file) = self.get_file(query).await {
            if file.mime_type == FOLDER_MIME && !file.trashed {
                return Ok(vec![file]);
            }
        }
        let escaped = query.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!("mimeType = '{FOLDER_MIME}' and name = '{escaped}' and trashed = false");
        let mut out = Vec::new();
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

    /// Query Drive storage quota: returns (used_bytes, total_limit_bytes).
    pub async fn about_storage(&self) -> Result<(u64, u64)> {
        let resp = self.get(&format!("{API}/about"), &[("fields", "storageQuota")]).await?;
        let about: AboutResponse = resp.json().await?;
        let quota = about.storage_quota.unwrap_or(StorageQuota { limit: None, usage: None });
        let usage = quota.usage.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let limit = quota.limit.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(10 * 1024 * 1024 * 1024 * 1024); // 10 TB fallback if unlimited
        Ok((usage, limit))
    }

    /// Creates a folder in Drive.
    pub async fn create_remote_folder(&self, name: &str, parent_id: &str) -> Result<RemoteFile> {
        let body = serde_json::json!({
            "name": name,
            "parents": [parent_id],
            "mimeType": FOLDER_MIME
        });
        let resp = self.post_json(&format!("{API}/files"), &[("fields", FILE_FIELDS)], &body).await?;
        Ok(resp.json().await?)
    }

    /// Moves a file to Google Drive trash.
    pub async fn trash_remote_file(&self, file_id: &str) -> Result<()> {
        let body = serde_json::json!({ "trashed": true });
        self.patch_json(&format!("{API}/files/{file_id}"), &[], &body).await?;
        Ok(())
    }

    /// Renames a remote file or folder.
    pub async fn rename_remote_file(&self, file_id: &str, new_name: &str) -> Result<RemoteFile> {
        let body = serde_json::json!({ "name": new_name });
        let resp = self.patch_json(&format!("{API}/files/{file_id}"), &[("fields", FILE_FIELDS)], &body).await?;
        Ok(resp.json().await?)
    }

    /// Moves a remote file to a different folder.
    pub async fn move_remote_file(&self, file_id: &str, new_parent: &str, old_parent: &str) -> Result<RemoteFile> {
        let body = serde_json::json!({});
        let query = [
            ("addParents", new_parent),
            ("removeParents", old_parent),
            ("fields", FILE_FIELDS),
        ];
        let resp = self.patch_json(&format!("{API}/files/{file_id}"), &query, &body).await?;
        Ok(resp.json().await?)
    }

    /// Creates a new file in Drive with binary contents (multipart/related upload).
    pub async fn create_remote_file(&self, name: &str, parent_id: &str, mime: &str, data: Vec<u8>) -> Result<RemoteFile> {
        let boundary = "boundary_gdsync_upload";
        let metadata = serde_json::json!({
            "name": name,
            "parents": [parent_id]
        });
        let metadata_str = serde_json::to_string(&metadata)?;

        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata_str}\r\n").as_bytes());
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let ct = format!("multipart/related; boundary={boundary}");
        let resp = self.request_bytes(
            reqwest::Method::POST,
            &format!("{UPLOAD_API}/files"),
            &[("uploadType", "multipart"), ("fields", FILE_FIELDS)],
            Some((body, ct)),
        ).await?;
        Ok(resp.json().await?)
    }

    /// Updates existing file content in Drive (media upload).
    pub async fn update_remote_file(&self, file_id: &str, mime: &str, data: Vec<u8>) -> Result<RemoteFile> {
        let resp = self.request_bytes(
            reqwest::Method::PATCH,
            &format!("{UPLOAD_API}/files/{file_id}"),
            &[("uploadType", "media"), ("fields", FILE_FIELDS)],
            Some((data, mime.to_string())),
        ).await?;
        Ok(resp.json().await?)
    }

    /// Inicia una sesión de subida resumible en Google Drive y devuelve la URL de sesión.
    async fn start_resumable_session(
        &self,
        method: reqwest::Method,
        url: &str,
        query: &[(&str, &str)],
        mime: &str,
        total_size: u64,
        metadata: &serde_json::Value,
    ) -> Result<String> {
        let bearer = self.bearer().await?;
        let body_bytes = serde_json::to_vec(metadata)?;
        let resp = self
            .http
            .request(method, url)
            .bearer_auth(bearer)
            .query(query)
            .header("X-Upload-Content-Type", mime)
            .header("X-Upload-Content-Length", total_size.to_string())
            .header(reqwest::header::CONTENT_TYPE, "application/json; charset=UTF-8")
            .body(body_bytes)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("falló al iniciar subida resumible ({status}): {body}");
        }

        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .context("la respuesta no contiene cabecera Location para subida resumible")?
            .to_str()?
            .to_string();

        Ok(loc)
    }

    /// Ejecuta la subida de un archivo en bloques de 8 MB con soporte para reanudación ante cortes.
    async fn execute_resumable_upload(
        &self,
        session_url: &str,
        file_path: &Path,
        total_size: u64,
    ) -> Result<RemoteFile> {
        const CHUNK_SIZE: u64 = 8 * 1024 * 1024; // 8 MB por bloque
        let mut file = tokio::fs::File::open(file_path).await?;
        let mut uploaded = 0u64;

        if total_size == 0 {
            let bearer = self.bearer().await?;
            let resp = self
                .http
                .put(session_url)
                .bearer_auth(bearer)
                .header(reqwest::header::CONTENT_LENGTH, "0")
                .header("Content-Range", "bytes */0")
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("error en subida resumible de archivo vacío ({status}): {body}");
            }
            return Ok(resp.json().await?);
        }

        while uploaded < total_size {
            let chunk_len = (total_size - uploaded).min(CHUNK_SIZE);
            file.seek(std::io::SeekFrom::Start(uploaded)).await?;
            let mut buf = vec![0u8; chunk_len as usize];
            file.read_exact(&mut buf).await?;

            let range_end = uploaded + chunk_len - 1;
            let range_header = format!("bytes {uploaded}-{range_end}/{total_size}");

            let mut attempt = 0;
            let mut delay = Duration::from_millis(500);

            loop {
                let bearer = self.bearer().await?;
                let resp = self
                    .http
                    .put(session_url)
                    .bearer_auth(&bearer)
                    .header(reqwest::header::CONTENT_LENGTH, chunk_len.to_string())
                    .header("Content-Range", &range_header)
                    .body(buf.clone())
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let remote: RemoteFile = r.json().await?;
                        return Ok(remote);
                    }
                    Ok(r) if r.status().as_u16() == 308 => {
                        if let Some(range) = r.headers().get("Range") {
                            if let Ok(s) = range.to_str() {
                                if let Some(last) = s.split('-').nth(1).and_then(|v| v.parse::<u64>().ok()) {
                                    uploaded = last + 1;
                                    break;
                                }
                            }
                        }
                        uploaded += chunk_len;
                        break;
                    }
                    Ok(r) if attempt < 5 && (r.status() == StatusCode::TOO_MANY_REQUESTS || r.status().is_server_error()) => {
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                    Err(_) if attempt < 5 => {
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        delay *= 2;

                        // Consultar estado de bytes recibidos por Google Drive
                        if let Ok(bearer) = self.bearer().await {
                            if let Ok(status_resp) = self
                                .http
                                .put(session_url)
                                .bearer_auth(bearer)
                                .header(reqwest::header::CONTENT_LENGTH, "0")
                                .header("Content-Range", format!("bytes */{total_size}"))
                                .send()
                                .await
                            {
                                if status_resp.status().as_u16() == 308 {
                                    if let Some(range) = status_resp.headers().get("Range") {
                                        if let Ok(s) = range.to_str() {
                                            if let Some(last) = s.split('-').nth(1).and_then(|v| v.parse::<u64>().ok()) {
                                                uploaded = last + 1;
                                                break;
                                            }
                                        }
                                    }
                                } else if status_resp.status().is_success() {
                                    let remote: RemoteFile = status_resp.json().await?;
                                    return Ok(remote);
                                }
                            }
                        }
                    }
                    Ok(r) => {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        bail!("error durante subida resumible ({status}): {body}");
                    }
                    Err(e) => {
                        bail!("falló la conexión durante la subida en chunk {range_header}: {e}");
                    }
                }
            }
        }

        let bearer = self.bearer().await?;
        let resp = self
            .http
            .put(session_url)
            .bearer_auth(bearer)
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .header("Content-Range", format!("bytes */{total_size}"))
            .send()
            .await?;
        if resp.status().is_success() {
            let remote: RemoteFile = resp.json().await?;
            Ok(remote)
        } else {
            bail!("subida finalizada pero no se recibió confirmación de Google Drive");
        }
    }

    /// Sube un archivo a Drive eligiendo automáticamente entre subida directa
    /// (multipart) para archivos pequeños (< 5 MB) o subida resumible en bloques para archivos grandes.
    pub async fn upload_file_path(
        &self,
        name: &str,
        parent_id: &str,
        mime: &str,
        file_path: &Path,
    ) -> Result<RemoteFile> {
        let meta = tokio::fs::metadata(file_path).await?;
        let size = meta.len();

        if size < 5 * 1024 * 1024 {
            let data = if size > 0 {
                tokio::fs::read(file_path).await?
            } else {
                Vec::new()
            };
            return self.create_remote_file(name, parent_id, mime, data).await;
        }

        let metadata = serde_json::json!({
            "name": name,
            "parents": [parent_id]
        });
        let session_url = self
            .start_resumable_session(
                reqwest::Method::POST,
                &format!("{UPLOAD_API}/files"),
                &[("uploadType", "resumable"), ("fields", FILE_FIELDS)],
                mime,
                size,
                &metadata,
            )
            .await?;

        self.execute_resumable_upload(&session_url, file_path, size).await
    }

    /// Actualiza el contenido de un archivo en Drive usando subida directa para archivos
    /// pequeños o subida resumible en bloques para archivos grandes.
    pub async fn update_file_path(
        &self,
        file_id: &str,
        mime: &str,
        file_path: &Path,
    ) -> Result<RemoteFile> {
        let meta = tokio::fs::metadata(file_path).await?;
        let size = meta.len();

        if size < 5 * 1024 * 1024 {
            let data = if size > 0 {
                tokio::fs::read(file_path).await?
            } else {
                Vec::new()
            };
            return self.update_remote_file(file_id, mime, data).await;
        }

        let metadata = serde_json::json!({});
        let session_url = self
            .start_resumable_session(
                reqwest::Method::PATCH,
                &format!("{UPLOAD_API}/files/{file_id}"),
                &[("uploadType", "resumable"), ("fields", FILE_FIELDS)],
                mime,
                size,
                &metadata,
            )
            .await?;

        self.execute_resumable_upload(&session_url, file_path, size).await
    }
}
