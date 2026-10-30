//! OAuth 2.0 for installed apps: loopback redirect + PKCE.
use crate::config::{config_dir, write_private, Config};
use crate::now;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive";

// shortcut: token stored in a 0600 file; move to KWallet/Secret Service (keyring crate) later.
#[derive(Serialize, Deserialize, Clone)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

impl Token {
    pub fn path() -> PathBuf {
        config_dir().join("token.json")
    }
    pub fn load() -> Result<Self> {
        let s = std::fs::read_to_string(Self::path())
            .context("no hay sesión iniciada; ejecutá `gdsync login`")?;
        Ok(serde_json::from_str(&s)?)
    }
    pub fn save(&self) -> Result<()> {
        write_private(&Self::path(), &serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = http.post(TOKEN_URL).form(form).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Google rechazó el pedido de token ({status}): {}", resp.text().await.unwrap_or_default());
    }
    Ok(resp.json().await?)
}

/// Interactive login: opens the browser and waits for the redirect on 127.0.0.1.
pub async fn login(cfg: &Config) -> Result<Token> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = random_urlsafe(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(16);

    let url = reqwest::Url::parse_with_params(
        AUTH_URL,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )?;
    println!("Abriendo el navegador para iniciar sesión. Si no se abre, entrá a:\n\n{url}\n");
    let _ = std::process::Command::new("xdg-open").arg(url.as_str()).spawn();

    let params = wait_for_redirect(&listener).await?;
    if let Some(err) = params.get("error") {
        bail!("Google devolvió un error: {err}");
    }
    if params.get("state") != Some(&state) {
        bail!("el parámetro `state` no coincide; login abortado");
    }
    let code = params.get("code").context("Google no devolvió el código")?;

    let http = reqwest::Client::new();
    let resp = token_request(
        &http,
        &[
            ("code", code.as_str()),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ],
    )
    .await?;
    let token = Token {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token.context("Google no devolvió refresh_token")?,
        expires_at: now() + resp.expires_in,
    };
    token.save()?;
    Ok(token)
}

/// Accepts connections until one carries the OAuth redirect (browsers may open
/// speculative connections that never send data, or ask for /favicon.ico).
async fn wait_for_redirect(listener: &TcpListener) -> Result<HashMap<String, String>> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(3), sock.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let Some(path) = req.lines().next().and_then(|l| l.split_whitespace().nth(1)) else {
            continue;
        };
        let url = reqwest::Url::parse(&format!("http://localhost{path}"))?;
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        if !params.contains_key("code") && !params.contains_key("error") {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let body = "<html><body style='font-family:sans-serif'><h2>gdsync: listo, ya podés cerrar esta pestaña.</h2></body></html>";
        let _ = sock
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return Ok(params);
    }
}

pub async fn refresh(http: &reqwest::Client, cfg: &Config, tok: &Token) -> Result<Token> {
    let resp = token_request(
        http,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", tok.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ],
    )
    .await?;
    let new = Token {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token.unwrap_or_else(|| tok.refresh_token.clone()),
        expires_at: now() + resp.expires_in,
    };
    new.save()?;
    Ok(new)
}

pub async fn revoke(tok: &Token) -> Result<()> {
    reqwest::Client::new()
        .post(REVOKE_URL)
        .form(&[("token", tok.refresh_token.as_str())])
        .send()
        .await?;
    Ok(())
}
