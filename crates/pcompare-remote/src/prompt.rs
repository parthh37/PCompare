//! Questions a server asks while signing in that no saved setting answers —
//! SSH keyboard-interactive prompts such as a one-time code, or a password
//! that was never saved (or turned out to be wrong).
//!
//! Connections are opened on worker threads deep inside an engine, so the
//! app installs a [`Prompter`] that forwards the question to the UI and
//! blocks until the user answers or gives up.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptField {
    pub prompt: String,
    /// Whether the answer may be shown while typed (false for passwords).
    pub echo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPrompt {
    /// The connection profile's display name.
    pub profile: String,
    /// `user@host`.
    pub target: String,
    /// Title and instructions sent by the server (often empty).
    pub name: String,
    pub instructions: String,
    pub fields: Vec<PromptField>,
}

pub trait Prompter: Send + Sync {
    /// Asks the user; one answer per field, or `None` if they cancelled.
    fn ask(&self, prompt: &AuthPrompt) -> Option<Vec<String>>;
}

/// Whether a prompt asks for the account password (which a saved password
/// can answer) rather than, say, a verification code.
pub fn is_password_prompt(field: &PromptField) -> bool {
    let p = field.prompt.to_lowercase();
    !field.echo
        && (p.contains("password") || p.contains("passphrase") || p.contains("passwort") || p.contains("mot de passe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_password_prompts() {
        let f = |prompt: &str, echo| PromptField {
            prompt: prompt.into(),
            echo,
        };
        assert!(is_password_prompt(&f("Password: ", false)));
        assert!(is_password_prompt(&f("me@host's password:", false)));
        assert!(!is_password_prompt(&f("Verification code: ", false)));
        assert!(!is_password_prompt(&f("Password (shown): ", true)));
    }
}
