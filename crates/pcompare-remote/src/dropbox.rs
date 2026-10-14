//! Dropbox specifics the OpenDAL service doesn't cover: team spaces,
//! checking the root folder, and readable errors.
//!
//! In a Dropbox team space every call starts in the member's own folder
//! (the "home namespace"); team folders live in the team's root namespace
//! and are only reachable when calls carry a `Dropbox-API-Path-Root` header
//! naming it. See <https://developers.dropbox.com/dbx-team-files-guide>.

use crate::rt;
use pcompare_core::{CoreError, Result};
use serde::Deserialize;
use serde_json::Value;

const API: &str = "https://api.dropboxapi.com/2";

/// Permissions PCompare asks for (and the ones a pasted token needs).
pub const SCOPES: &str = "files.metadata.read files.content.read files.content.write";

#[derive(Deserialize)]
struct Account {
    root_info: RootInfo,
}

#[derive(Deserialize)]
struct RootInfo {
    root_namespace_id: String,
    home_namespace_id: String,
}

/// Talks to Dropbox before OpenDAL does: finds the namespace paths should
/// be relative to and checks that `root` is a folder there. Returns the
/// team's root namespace for team-space accounts, `None` for everyone
/// else (paths then start in the member's own folder, as before).
pub fn prepare(token: &str, root: &str) -> Result<Option<String>> {
    let http = reqwest::Client::new();
    rt().block_on(async {
        let account: Account = call(&http, token, None, "users/get_current_account", None).await?;
        let info = account.root_info;
        let namespace = (info.root_namespace_id != info.home_namespace_id).then_some(info.root_namespace_id);

        let root = root.trim().trim_end_matches('/');
        if !root.is_empty() {
            let path = if root.starts_with('/') { root.to_owned() } else { format!("/{root}") };
            let meta: Value = call(
                &http,
                token,
                namespace.as_deref(),
                "files/get_metadata",
                Some(serde_json::json!({ "path": path })),
            )
            .await
            .map_err(|e| match e {
                CoreError::Remote { message, .. } if message.contains("not_found") => CoreError::Other(format!(
                    "the Root folder `{path}` doesn't exist in this Dropbox{} — click Browse to pick one",
                    if namespace.is_some() { " (or its team space)" } else { "" }
                )),
                e => e,
            })?;
            if meta[".tag"] != "folder" {
                return Err(CoreError::Other(format!("the Root folder `{path}` is a file, not a folder")));
            }
        }
        Ok(namespace)
    })
}

/// One RPC call. `namespace` becomes the `Dropbox-API-Path-Root` header.
async fn call<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    namespace: Option<&str>,
    endpoint: &str,
    args: Option<Value>,
) -> Result<T> {
    let mut req = http.post(format!("{API}/{endpoint}")).bearer_auth(token);
    if let Some(ns) = namespace {
        req = req.header("Dropbox-API-Path-Root", path_root(ns));
    }
    // Calls without arguments must not send a body (not even `null`).
    if let Some(args) = args {
        req = req.json(&args);
    }
    let fail = |e: &dyn std::fmt::Display| CoreError::remote("Dropbox", e);
    let resp = req.send().await.map_err(|e| fail(&e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| fail(&e))?;
    if !status.is_success() {
        return Err(match explain(&body) {
            Some(advice) => CoreError::Other(advice),
            None => CoreError::remote("Dropbox", body),
        });
    }
    serde_json::from_str(&body).map_err(|e| fail(&e))
}

/// The `Dropbox-API-Path-Root` header value for a namespace.
pub fn path_root(namespace: &str) -> String {
    format!(r#"{{".tag":"root","root":"{namespace}"}}"#)
}

/// Turns the Dropbox error inside `message` (a raw API error body, possibly
/// wrapped in OpenDAL's context) into advice, when it's one people can fix.
pub fn explain(message: &str) -> Option<String> {
    let start = message.find(r#"{"error_summary""#)?;
    let body: Value = serde_json::Deserializer::from_str(&message[start..])
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let error = &body["error"];
    let text = match error[".tag"].as_str()? {
        "missing_scope" => {
            let scope = error["required_scope"].as_str().unwrap_or("files.metadata.read");
            format!(
                "This Dropbox token doesn't have the `{scope}` permission. In the Dropbox App Console open your app's \
                 Permissions tab, tick {}, and click Submit. Then generate a new access token or sign in again: \
                 a token keeps the permissions it was created with.",
                SCOPES.replace(' ', ", ")
            )
        }
        "expired_access_token" => "The Dropbox access token has expired (generated tokens last about 4 hours). \
             Generate a new one, or clear it and sign in instead — a sign-in renews itself."
            .into(),
        "invalid_access_token" => "Dropbox doesn't accept this access token. Check it was copied completely, \
             or clear it and sign in instead."
            .into(),
        "user_suspended" => "This Dropbox account is suspended.".into(),
        _ => return None,
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_a_missing_scope_wrapped_in_opendal_context() {
        let msg = r#"Unexpected (persistent) at list, context: { service: dropbox } => {"error_summary": "missing_scope/..", "error": {".tag": "missing_scope", "required_scope": "files.metadata.read"}}"#;
        let text = explain(msg).unwrap();
        assert!(text.contains("`files.metadata.read` permission"), "{text}");
        assert!(text.contains("generate a new access token"), "{text}");
    }

    #[test]
    fn explains_token_errors_and_ignores_others() {
        let expired = r#"{"error_summary": "expired_access_token/", "error": {".tag": "expired_access_token"}}"#;
        assert!(explain(expired).unwrap().contains("expired"));
        let not_found = r#"{"error_summary": "path/not_found/", "error": {".tag": "path", "path": {".tag": "not_found"}}}"#;
        assert_eq!(explain(not_found), None);
        assert_eq!(explain("plain text"), None);
    }

    #[test]
    fn team_space_accounts_use_the_root_namespace() {
        let team: Account = serde_json::from_str(
            r#"{"root_info": {".tag": "team", "root_namespace_id": "3235641", "home_namespace_id": "3235642", "home_path": "/Ann"}}"#,
        )
        .unwrap();
        assert_ne!(team.root_info.root_namespace_id, team.root_info.home_namespace_id);
        assert_eq!(path_root("3235641"), r#"{".tag":"root","root":"3235641"}"#);
    }
}
