//! OAuth 2.0 "Sign in" for OneDrive, Dropbox and Google Drive.
//!
//! Uses the authorization-code flow with PKCE and a loopback redirect, the
//! flow recommended for desktop apps (RFC 8252): we open the system browser,
//! catch the redirect on `http://localhost:53682/`, and exchange the code.
//! PKCE means no client secret is needed for OneDrive and Dropbox.

use crate::provider::OAuthSpec;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use pcompare_core::{CoreError, Result};
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Fixed so it can be registered exactly (Dropbox requires an exact match).
pub const REDIRECT_PORT: u16 = 53682;
pub const REDIRECT_URI: &str = "http://localhost:53682/";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..len).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn oauth_err(message: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("sign-in failed: {message}"))
}

/// The error a cancelled sign-in ends with; the UI matches on it.
pub const CANCELLED: &str = "sign-in cancelled";

/// Runs the interactive sign-in. `open_browser` is called with the
/// authorization URL; the future resolves once the browser redirects back,
/// or fails with [`CANCELLED`] as soon as `cancel` completes (which also
/// frees the redirect port for the next attempt).
pub async fn authorize(
    spec: &OAuthSpec,
    client_id: &str,
    client_secret: Option<&str>,
    open_browser: impl FnOnce(&str) -> Result<()>,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<Tokens> {
    let verifier = random_token(64);
    let state = random_token(24);
    let mut params = vec![
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", spec.scopes),
        ("state", &state),
        ("code_challenge_method", "S256"),
    ];
    let challenge = pkce_challenge(&verifier);
    params.push(("code_challenge", &challenge));
    params.extend(spec.extra_auth_params.iter().copied());
    let url = reqwest::Url::parse_with_params(spec.auth_url, &params).map_err(oauth_err)?;

    // Bind before opening the browser so the redirect can't race us.
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .await
        .map_err(|e| oauth_err(format!("port {REDIRECT_PORT} is busy ({e}); close other sign-in windows")))?;
    open_browser(url.as_str())?;

    let code = tokio::select! {
        waited = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(&listener, &state)) => {
            waited.map_err(|_| oauth_err("timed out waiting for the browser"))??
        }
        () = cancel => return Err(CoreError::Other(CANCELLED.into())),
    };
    drop(listener);

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", client_id),
        ("code_verifier", verifier.as_str()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    token_request(spec.token_url, &form).await
}

/// Trades a refresh token for a fresh access token.
pub async fn refresh(
    spec: &OAuthSpec,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<Tokens> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    token_request(spec.token_url, &form).await
}

async fn token_request(url: &str, form: &[(&str, &str)]) -> Result<Tokens> {
    let resp = reqwest::Client::new()
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(oauth_err)?;
    let status = resp.status();
    let body = resp.text().await.map_err(oauth_err)?;
    if !status.is_success() {
        return Err(oauth_err(format!("{status}: {body}")));
    }
    serde_json::from_str(&body).map_err(oauth_err)
}

/// Accepts browser connections until one carries our `state` and a `code`.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(oauth_err)?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or("/");
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).map_err(oauth_err)?;
        let get = |k: &str| url.query_pairs().find(|(q, _)| q == k).map(|(_, v)| v.into_owned());

        let (outcome, page) = match (get("state"), get("code"), get("error")) {
            (Some(s), Some(code), _) if s == state => (Some(Ok(code)), "Signed in. You can close this tab and return to PCompare."),
            (_, _, Some(err)) => (
                Some(Err(oauth_err(get("error_description").unwrap_or(err)))),
                "Sign-in was cancelled or failed. You can close this tab.",
            ),
            // favicon.ico and other stray requests: answer and keep waiting.
            _ => (None, ""),
        };
        let html = format!("<!doctype html><title>PCompare</title><body style=\"font-family:system-ui;padding:3em\"><h2>{page}</h2>");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = sock.write_all(reply.as_bytes()).await;
        if let Some(result) = outcome {
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        // Appendix B of RFC 7636.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn cancelling_stops_waiting_and_frees_the_port() {
        let spec = crate::provider::spec(crate::provider::ProviderKind::Dropbox).oauth.unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let run = authorize(&spec, "id", None, |_| Ok(()), async {
                let _ = rx.await;
            });
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = tx.send(());
            };
            let (result, ()) = tokio::join!(run, cancel);
            assert_eq!(result.unwrap_err().to_string(), CANCELLED);
            // The next attempt can bind the redirect port again.
            TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).await.unwrap();
        });
    }

    #[test]
    fn verifier_uses_unreserved_chars() {
        let v = random_token(64);
        assert_eq!(v.len(), 64);
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }
}
