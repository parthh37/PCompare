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
    Sharepoint,
    Github,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldKind {
    Text,
    /// Sealed in `secrets.vault`, never written in plain text or sent back to the UI.
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
    /// Tucked away under "Advanced" in the form.
    pub advanced: bool,
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
    /// PCompare's own app registration, compiled into release builds. With
    /// one, users just sign in; without (dev builds), each user registers
    /// their own app and enters its Client ID.
    #[serde(skip)]
    pub builtin: Option<BuiltinClient>,
    /// Tells the UI a Client ID is optional.
    pub has_builtin_client: bool,
}

/// An OAuth client shipped inside the app. For desktop apps neither half is
/// confidential (RFC 8252 §8.5; Google says the same of its "Desktop app"
/// secrets) — they identify the app, PKCE protects the sign-in.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinClient {
    pub id: &'static str,
    pub secret: Option<&'static str>,
}

/// Release builds set these from repository variables (see release.yml);
/// CI passes unset ones as empty strings, which count as absent.
const fn builtin(id: Option<&'static str>, secret: Option<&'static str>, needs_secret: bool) -> Option<BuiltinClient> {
    let secret = match secret {
        Some(s) if !s.is_empty() => Some(s),
        _ => None,
    };
    match id {
        Some(id) if !id.is_empty() && (secret.is_some() || !needs_secret) => Some(BuiltinClient { id, secret }),
        _ => None,
    }
}

/// One Microsoft Entra app serves OneDrive and SharePoint.
const MICROSOFT_APP: Option<BuiltinClient> = builtin(option_env!("PCOMPARE_MICROSOFT_CLIENT_ID"), None, false);
const DROPBOX_APP: Option<BuiltinClient> = builtin(option_env!("PCOMPARE_DROPBOX_CLIENT_ID"), None, false);
/// Google's token endpoint wants the secret even with PKCE.
const GOOGLE_APP: Option<BuiltinClient> = builtin(
    option_env!("PCOMPARE_GOOGLE_CLIENT_ID"),
    option_env!("PCOMPARE_GOOGLE_CLIENT_SECRET"),
    true,
);

const MICROSOFT_CONSOLE: &str = "https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade";
const MICROSOFT_AUTH: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const MICROSOFT_TOKEN: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";

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
/// Secret key of an access token pasted instead of signing in.
pub const ACCESS_TOKEN: &str = "access_token";

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
        advanced: false,
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
    const fn advanced(mut self) -> Self {
        self.advanced = true;
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

/// Client ID (and secret) fields for a sign-in provider: required when the
/// build has no app of its own, otherwise an optional override.
fn oauth_client_fields(with_secret: bool, builtin: Option<BuiltinClient>) -> Vec<FieldSpec> {
    let own = builtin.is_none();
    let mut id = field("client_id", "Client ID", Text);
    id = if own {
        id.required().help("From your app registration (see the link below).")
    } else {
        id.advanced()
            .placeholder("PCompare's app")
            .help("Only to sign in through an app registration of your own.")
    };
    let mut v = vec![id];
    if with_secret {
        let secret = field("client_secret", "Client secret", Secret);
        v.push(if own { secret.required() } else { secret.advanced() });
    }
    v
}

fn oauth_fields(with_secret: bool, builtin: Option<BuiltinClient>) -> Vec<FieldSpec> {
    let mut v = oauth_client_fields(with_secret, builtin);
    v.push(root());
    v
}

const fn oauth(
    auth_url: &'static str,
    token_url: &'static str,
    scopes: &'static str,
    extra_auth_params: &'static [(&'static str, &'static str)],
    console_url: &'static str,
    builtin: Option<BuiltinClient>,
) -> Option<OAuthSpec> {
    Some(OAuthSpec {
        auth_url,
        token_url,
        scopes,
        extra_auth_params,
        console_url,
        builtin,
        has_builtin_client: builtin.is_some(),
    })
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
            fields: oauth_fields(false, MICROSOFT_APP),
            oauth: oauth(
                MICROSOFT_AUTH,
                MICROSOFT_TOKEN,
                "offline_access Files.ReadWrite",
                &[],
                MICROSOFT_CONSOLE,
                MICROSOFT_APP,
            ),
        },
        ProviderSpec {
            kind: ProviderKind::Sharepoint,
            label: "Microsoft SharePoint",
            description: "A document library on a SharePoint Online or Microsoft Teams site, via Microsoft sign-in.",
            fields: {
                let mut v = vec![
                    field("site_url", "Site", Text)
                        .required()
                        .placeholder("https://contoso.sharepoint.com/sites/Marketing")
                        .help("Paste the site's address from the browser — a link into a library works too."),
                    field("library", "Library", Text)
                        .placeholder("Documents")
                        .help("The document library's name. Leave empty for the site's default library."),
                ];
                v.extend(oauth_fields(false, MICROSOFT_APP));
                v
            },
            // Sites.Read.All finds the site by its address; neither scope
            // needs a tenant admin's consent.
            oauth: oauth(
                MICROSOFT_AUTH,
                MICROSOFT_TOKEN,
                "offline_access Sites.Read.All Files.ReadWrite.All",
                &[],
                MICROSOFT_CONSOLE,
                MICROSOFT_APP,
            ),
        },
        ProviderSpec {
            kind: ProviderKind::Dropbox,
            label: "Dropbox",
            description: "Dropbox via Dropbox sign-in, or an access token you paste.",
            fields: {
                let mut v = oauth_client_fields(false, DROPBOX_APP);
                // Signing in needs a Client ID; a pasted token doesn't.
                v[0].required = false;
                v.push(
                    field(ACCESS_TOKEN, "Access token", Secret)
                        .placeholder("sl.… — instead of signing in")
                        .help("Paste a token generated on your app's page in the Dropbox App Console (Settings → Generated access token). Tick files.metadata.read, files.content.read and files.content.write on the Permissions tab first: a token keeps the permissions it was created with. Used instead of the sign-in while set. Generated tokens expire after about 4 hours; signing in doesn't."),
                );
                v.push(root());
                v
            },
            oauth: oauth(
                "https://www.dropbox.com/oauth2/authorize",
                "https://api.dropboxapi.com/oauth2/token",
                crate::dropbox::SCOPES,
                &[("token_access_type", "offline")],
                "https://www.dropbox.com/developers/apps",
                DROPBOX_APP,
            ),
        },
        ProviderSpec {
            kind: ProviderKind::Gdrive,
            label: "Google Drive",
            description: "Google Drive via Google sign-in.",
            fields: oauth_fields(true, GOOGLE_APP),
            oauth: oauth(
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                "https://www.googleapis.com/auth/drive",
                &[("access_type", "offline"), ("prompt", "consent")],
                "https://console.cloud.google.com/apis/credentials",
                GOOGLE_APP,
            ),
        },
        ProviderSpec {
            kind: ProviderKind::Github,
            label: "GitHub",
            description: "Repositories on GitHub or GitHub Enterprise, read-only. Branches and tags show up as folders, so you can compare one with another or with a local folder.",
            fields: vec![
                field("auth", "Sign in with", Select)
                    .options(&["token", "github-cli", "anonymous"])
                    .default("token"),
                field("token", "Personal access token", Secret)
                    .required()
                    .show_if("auth", "token")
                    .placeholder("github_pat_… or ghp_…")
                    .help("A fine-grained token with read-only Contents access to the repositories, or a classic token with the repo scope. Create one at github.com/settings/tokens."),
                field("owner", "Owner", Text)
                    .placeholder("All owners you can see")
                    .help("A user or organisation, to start browsing there."),
                field("repo", "Repository", Text)
                    .placeholder("All of the owner's repositories")
                    .help("Pin one repository: paths then start at its branches, e.g. main/src."),
                field("host", "Host", Text)
                    .advanced()
                    .placeholder("github.com")
                    .help("Your GitHub Enterprise Server's host name."),
            ],
            oauth: None,
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
            ProviderKind::Sharepoint,
            ProviderKind::Github,
        ];
        let cat = catalog();
        assert_eq!(cat.len(), all.len());
        for k in all {
            assert_eq!(cat.iter().filter(|p| p.kind == k).count(), 1, "{k:?}");
        }
    }

    #[test]
    fn builtin_clients_need_every_part() {
        assert!(builtin(None, None, false).is_none());
        assert!(builtin(Some(""), None, false).is_none(), "CI passes unset variables as empty");
        assert!(builtin(Some("id"), Some(""), true).is_none());
        let c = builtin(Some("id"), Some(""), false).unwrap();
        assert_eq!((c.id, c.secret), ("id", None));
        assert_eq!(builtin(Some("id"), Some("s"), true).unwrap().secret, Some("s"));
    }

    #[test]
    fn client_id_is_required_only_without_a_builtin_app() {
        let app = Some(BuiltinClient { id: "x", secret: None });
        let own = oauth_client_fields(true, None);
        assert!(own.iter().all(|f| f.required && !f.advanced));
        let shipped = oauth_client_fields(true, app);
        assert!(shipped.iter().all(|f| !f.required && f.advanced));
    }

    #[test]
    fn dropbox_takes_a_pasted_token_without_a_client_id() {
        let dropbox = spec(ProviderKind::Dropbox);
        let token = dropbox.fields.iter().find(|f| f.key == ACCESS_TOKEN).unwrap();
        assert_eq!(token.kind, FieldKind::Secret, "kept in the keychain");
        assert!(dropbox.fields.iter().all(|f| !f.required));
    }
}
