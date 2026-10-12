//! Notification channels: where a notification goes and how.

use serde::{Deserialize, Serialize};

/// One configured destination. Serialized to `notify-channels.json`
/// without secrets; see [`ChannelSecrets`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelConfig {
    pub id: String,
    /// Shown in the UI and in task settings, e.g. "Ops team (Teams)".
    pub name: String,
    pub enabled: bool,
    pub settings: ChannelSettings,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ChannelSettings {
    /// Microsoft Teams through an incoming webhook: a Teams "Workflows"
    /// (Power Automate) webhook or a legacy Office 365 connector URL. The
    /// URL is secret (`ChannelSecrets::url`). Posts an Adaptive Card.
    Teams {},
    /// Slack incoming webhook (`ChannelSecrets::url`). Posts Block Kit.
    Slack {},
    /// Any HTTP endpoint (`ChannelSecrets::url`): POSTs the [`crate::Notification`]
    /// as JSON. With `ChannelSecrets::signing_key` set, the body is signed
    /// with HMAC-SHA256 in an `X-PCompare-Signature: sha256=<hex>` header.
    #[serde(rename_all = "camelCase")]
    Webhook {
        /// Extra request headers (not secret; put tokens in the URL or use signing).
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    /// Email over SMTP. The password is `ChannelSecrets::password`.
    #[serde(rename_all = "camelCase")]
    Email {
        host: String,
        /// 0 = the default for `security` (465 TLS, 587 STARTTLS, 25 none).
        port: u16,
        security: EmailSecurity,
        /// Empty = no authentication.
        username: String,
        /// `Name <address>` or a bare address.
        from: String,
        to: Vec<String>,
        #[serde(default)]
        cc: Vec<String>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum EmailSecurity {
    /// Implicit TLS (SMTPS, usually port 465).
    Tls,
    /// Plain connection upgraded with STARTTLS (usually 587); required.
    #[default]
    StartTls,
    /// No encryption (port 25, internal relays only).
    None,
}

/// The secret parts of a channel, kept in the [`pcompare_core::secrets::SecretStore`]
/// under profile `notify:<channel id>` with keys `url`, `password`,
/// `signingKey`. When saving, `None` means "leave the stored value as is"
/// and `Some("")` means "remove it".
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSecrets {
    pub url: Option<String>,
    pub password: Option<String>,
    pub signing_key: Option<String>,
}

impl ChannelSettings {
    /// A short kind label for lists and logs: "Teams", "Email", ...
    pub fn kind_label(&self) -> &'static str {
        match self {
            ChannelSettings::Teams {} => "Teams",
            ChannelSettings::Slack {} => "Slack",
            ChannelSettings::Webhook { .. } => "Webhook",
            ChannelSettings::Email { .. } => "Email",
        }
    }
}
