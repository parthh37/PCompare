//! The catalog of supported remote vendors.
//!
//! Each provider declares its form fields here and the UI renders the
//! connection dialog from this list, so supporting a new vendor is one entry
//! in [`catalog`] plus (for OpenDAL-backed services) its scheme name.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    S3,
    Azblob,
    Gcs,
    B2,
    Ftp,
    Sftp,
    Webdav,
    Onedrive,
    Dropbox,
    Gdrive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldKind {
    Text,
    /// Stored in the OS keychain, never written to disk or sent back to the UI.
    Secret,
    Number,
    Checkbox,
    Select,
    /// A local file path (e.g. a private key) picked with a file dialog.
    File,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    pub placeholder: &'static str,
    pub help: &'static str,
    pub default: &'static str,
    pub options: &'static [&'static str],
    /// Only show this field when another field has a given value: `(key, value)`;
    /// `value` may list alternatives separated by `|`.
    pub show_if: Option<(&'static str, &'static str)>,
}

/// OAuth 2.0 authorization-code + PKCE endpoints for "Sign in" providers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSpec {
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static str,
    /// Extra query parameters some providers need to issue a refresh token.
    pub extra_auth_params: &'static [(&'static str, &'static str)],
    /// Where the user registers their own app to obtain a client id.
    pub console_url: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSpec {
    pub kind: ProviderKind,
    pub label: &'static str,
    pub description: &'static str,
    pub fields: Vec<FieldSpec>,
    pub oauth: Option<OAuthSpec>,
}

/// Secret key under which an OAuth refresh token is stored.
pub const REFRESH_TOKEN: &str = "refresh_token";

const fn field(key: &'static str, label: &'static str, kind: FieldKind) -> FieldSpec {
    FieldSpec {
        key,
        label,
        kind,
        required: false,
        placeholder: "",
        help: "",
        default: "",
        options: &[],
        show_if: None,
    }
}

impl FieldSpec {
    const fn required(mut self) -> Self {
        self.required = true;
        self
    }
    const fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = p;
        self
    }
    const fn help(mut self, h: &'static str) -> Self {
        self.help = h;
        self
    }
    const fn default(mut self, d: &'static str) -> Self {
        self.default = d;
        self
    }
    const fn options(mut self, o: &'static [&'static str]) -> Self {
        self.options = o;
        self
    }
    const fn show_if(mut self, key: &'static str, value: &'static str) -> Self {
        self.show_if = Some((key, value));
        self
    }
}

use FieldKind::*;

#[cfg(unix)]
const AGENT_PLACEHOLDER: &str = "$SSH_AUTH_SOCK";
#[cfg(unix)]
const AGENT_HELP: &str = "Keys held by ssh-agent are tried in turn. Leave empty to use SSH_AUTH_SOCK, or enter the agent's socket path.";
#[cfg(windows)]
const AGENT_PLACEHOLDER: &str = r"\\.\pipe\openssh-ssh-agent";
#[cfg(windows)]
const AGENT_HELP: &str =
    "Keys held by the agent are tried in turn. Leave empty to use the Windows OpenSSH Authentication Agent, or Pageant if that isn't running.";

fn root() -> FieldSpec {
    field("root", "Root folder", Text)
        .placeholder("/")
        .help("Paths are relative to this folder.")
}

fn oauth_client_fields(with_secret: bool) -> Vec<FieldSpec> {
    let mut v = vec![field("client_id", "Client ID", Text)
        .required()
        .help("From your app registration (see the link below).")];
    if with_secret {
        v.push(field("client_secret", "Client secret", Secret).required());
    }
    v.push(root());
    v
}

pub fn catalog() -> Vec<ProviderSpec> {
    vec![
        ProviderSpec {
            kind: ProviderKind::S3,
            label: "Amazon S3 / S3-compatible",
            description: "AWS S3, MinIO, Cloudflare R2, Wasabi, DigitalOcean Spaces…",
            fields: vec![
                field("bucket", "Bucket", Text).required(),
                field("region", "Region", Text).placeholder("us-east-1"),
                field("endpoint", "Endpoint", Text)
                    .placeholder("https://s3.amazonaws.com")
                    .help("Leave empty for AWS; set for R2, MinIO, Wasabi, etc."),
                field("access_key_id", "Access key ID", Text),
                field("secret_access_key", "Secret access key", Secret),
                root(),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::Azblob,
            label: "Azure Blob Storage",
            description: "Containers in an Azure storage account.",
            fields: vec![
                field("account_name", "Account name", Text).required(),
                field("container", "Container", Text).required(),
                field("account_key", "Account key", Secret),
                field("sas_token", "SAS token", Secret).help("Use instead of an account key."),
                field("endpoint", "Endpoint", Text)
                    .placeholder("https://<account>.blob.core.windows.net"),
                root(),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::Gcs,
            label: "Google Cloud Storage",
            description: "Buckets in Google Cloud.",
            fields: vec![
                field("bucket", "Bucket", Text).required(),
                field("credential_path", "Service account JSON", File)
                    .help("Key file for a service account with access to the bucket."),
                root(),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::B2,
            label: "Backblaze B2",
            description: "B2 cloud storage buckets.",
            fields: vec![
                field("bucket", "Bucket", Text).required(),
                field("bucket_id", "Bucket ID", Text).required(),
                field("application_key_id", "Application key ID", Text).required(),
                field("application_key", "Application key", Secret).required(),
                root(),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::Ftp,
            label: "FTP / FTPS",
            description: "Classic FTP servers, with optional TLS.",
            fields: vec![
                field("host", "Host", Text).required().placeholder("ftp.example.com"),
                field("port", "Port", Number).default("21"),
                field("secure", "Use TLS (FTPS)", Checkbox).default("true"),
                field("user", "User", Text).placeholder("anonymous"),
                field("password", "Password", Secret),
                root(),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::Sftp,
            label: "SFTP (SSH)",
            description: "Any SSH server with the SFTP subsystem.",
            fields: vec![
                field("host", "Host", Text).required().placeholder("server.example.com"),
                field("port", "Port", Number).default("22"),
                field("user", "User", Text).required(),
                field("auth", "Authentication", Select)
                    .options(&["password", "keyboard-interactive", "key", "agent"])
                    .default("password"),
                field("password", "Password", Secret)
                    .show_if("auth", "password|keyboard-interactive")
                    .help("Answers password prompts. Other questions (e.g. a verification code) are asked when connecting."),
                field("private_key_path", "Private key file", File)
                    .show_if("auth", "key")
                    .placeholder("~/.ssh/id_ed25519"),
                field("passphrase", "Key passphrase", Secret).show_if("auth", "key"),
                field("agent_socket", "Agent", Text)
                    .show_if("auth", "agent")
                    .placeholder(AGENT_PLACEHOLDER)
                    .help(AGENT_HELP),
                field("root", "Root folder", Text)
                    .placeholder("~ (home directory)")
                    .help("Absolute path, or relative to the home directory."),
                field("host_key", "Trusted host key", Text).help(
                    "Filled in on first connect; a different key later is refused. Clear it to re-trust.",
                ),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::Webdav,
            label: "WebDAV",
            description: "Nextcloud, ownCloud, SharePoint WebDAV, Apache mod_dav…",
            fields: vec![
                field("endpoint", "URL", Text)
                    .required()
                    .placeholder("https://cloud.example.com/remote.php/dav/files/me"),
                field("username", "Username", Text),
                field("password", "Password", Secret),
                field("token", "Bearer token", Secret).help("Use instead of username/password."),
                root(),
            ],
            oauth: None,
        },
        ProviderSpec {
            kind: ProviderKind::Onedrive,
            label: "Microsoft OneDrive",
            description: "Personal or work OneDrive via Microsoft sign-in.",
            fields: oauth_client_fields(false),
            oauth: Some(OAuthSpec {
                auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                scopes: "offline_access Files.ReadWrite",
                extra_auth_params: &[],
                console_url: "https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade",
            }),
        },
        ProviderSpec {
            kind: ProviderKind::Dropbox,
            label: "Dropbox",
            description: "Dropbox via Dropbox sign-in.",
            fields: oauth_client_fields(false),
            oauth: Some(OAuthSpec {
                auth_url: "https://www.dropbox.com/oauth2/authorize",
                token_url: "https://api.dropboxapi.com/oauth2/token",
                scopes: "files.metadata.read files.content.read files.content.write",
                extra_auth_params: &[("token_access_type", "offline")],
                console_url: "https://www.dropbox.com/developers/apps",
            }),
        },
        ProviderSpec {
            kind: ProviderKind::Gdrive,
            label: "Google Drive",
            description: "Google Drive via Google sign-in.",
            fields: oauth_client_fields(true),
            oauth: Some(OAuthSpec {
                auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
                token_url: "https://oauth2.googleapis.com/token",
                scopes: "https://www.googleapis.com/auth/drive",
                extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
                console_url: "https://console.cloud.google.com/apis/credentials",
            }),
        },
    ]
}

pub fn spec(kind: ProviderKind) -> ProviderSpec {
    catalog()
        .into_iter()
        .find(|p| p.kind == kind)
        .expect("every ProviderKind has a catalog entry")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_exactly_one_entry() {
        let all = [
            ProviderKind::S3,
            ProviderKind::Azblob,
            ProviderKind::Gcs,
            ProviderKind::B2,
            ProviderKind::Ftp,
            ProviderKind::Sftp,
            ProviderKind::Webdav,
            ProviderKind::Onedrive,
            ProviderKind::Dropbox,
            ProviderKind::Gdrive,
        ];
        let cat = catalog();
        assert_eq!(cat.len(), all.len());
        for k in all {
            assert_eq!(cat.iter().filter(|p| p.kind == k).count(), 1, "{k:?}");
        }
    }
}
