//! Notifications for PCompare's sync automation: when an automated sync
//! fails (or recovers, or finishes with warnings), tell people where they
//! already are — a Microsoft Teams channel, an email inbox, Slack, or any
//! HTTP endpoint.
//!
//! Like the other engine crates this one is UI-agnostic. Channel settings
//! live in a JSON file ([`ChannelStore`]); everything secret — webhook URLs
//! (which embed their own credentials), SMTP passwords, HMAC keys — goes
//! through a [`SecretStore`] (the OS keychain in the app), never the file.
//!
//! Delivery is synchronous and bounded: every transport has connect and
//! overall timeouts, and transient failures (HTTP 429/5xx, connection
//! errors, SMTP 4xx) are retried a few times with backoff, honouring
//! `Retry-After`. A notification that still can't be delivered is reported
//! back as a [`NotifyError`] so the caller can record it; it never panics
//! and never blocks a sync.
//!
//! [`send`] blocks the calling thread (worst case a couple of minutes), so
//! call it from a plain thread or `spawn_blocking`, never from inside an
//! async task: the HTTP client runs its own small runtime and refuses to be
//! driven from within another one.

pub mod channel;
pub mod email;
pub mod http;
pub mod message;
pub mod store;

use std::thread;
use std::time::Duration;

use pcompare_core::secrets::SecretStore;

pub use channel::{ChannelConfig, ChannelSecrets, ChannelSettings, EmailSecurity};
pub use message::{Fact, FailureLine, Notification, NotifyReason, Severity};
pub use store::{ChannelStore, SecretsPresent};

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// The channel's settings are incomplete or invalid; retrying won't help.
    #[error("{0}")]
    Config(String),
    /// A secret (webhook URL, password) the channel needs isn't stored.
    #[error("the {0} for this channel isn't set")]
    MissingSecret(&'static str),
    /// The receiving service answered with an error.
    #[error("{service} answered {status}: {body}")]
    Rejected {
        service: &'static str,
        status: u16,
        body: String,
    },
    /// Network, TLS or SMTP transport failure (after retries).
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Core(#[from] pcompare_core::CoreError),
}

pub type Result<T> = std::result::Result<T, NotifyError>;

/// Sends `n` through one channel, retrying transient failures. Disabled
/// channels are not skipped here — that is the caller's decision (a "Send
/// test" button must work on a disabled channel).
pub fn send(channel: &ChannelConfig, secrets: &dyn SecretStore, n: &Notification) -> Result<()> {
    let profile = store::secret_profile(&channel.id);
    match &channel.settings {
        ChannelSettings::Teams {} => {
            let url = required_secret(secrets, &profile, store::KEY_URL, "webhook URL")?;
            http::send_teams(&url, n)
        }
        ChannelSettings::Slack {} => {
            let url = required_secret(secrets, &profile, store::KEY_URL, "webhook URL")?;
            http::send_slack(&url, n)
        }
        ChannelSettings::Webhook { headers } => {
            let url = required_secret(secrets, &profile, store::KEY_URL, "webhook URL")?;
            let key = optional_secret(secrets, &profile, store::KEY_SIGNING)?;
            http::send_webhook(&url, headers, key.as_deref(), n)
        }
        ChannelSettings::Email {
            host,
            port,
            security,
            username,
            from,
            to,
            cc,
        } => {
            let password = if username.trim().is_empty() {
                None
            } else {
                Some(required_secret(
                    secrets,
                    &profile,
                    store::KEY_PASSWORD,
                    "password",
                )?)
            };
            let account = email::SmtpAccount {
                host,
                port: *port,
                security: *security,
                username,
                password: password.as_deref(),
            };
            email::send_email(&account, from, to, cc, n)
        }
    }
}

/// A sample notification for the "Send test" button: shaped like a real
/// failure report (facts, a failed file) so the formatting can be judged,
/// but clearly labelled as a test. `host` is left empty for the caller to
/// fill in (it knows the machine name); an empty host is simply omitted.
pub fn test_notification(channel_name: &str) -> Notification {
    let now = pcompare_core::store::now_ms();
    Notification {
        reason: NotifyReason::Test,
        severity: Severity::Info,
        title: "Test notification from PCompare".into(),
        summary: format!(
            "If you can read this, the \u{201c}{channel_name}\u{201d} channel works. Real notifications look like \
             this one and are sent when an automated sync fails, finishes with warnings or recovers."
        ),
        task_id: String::new(),
        task_name: "Example: Nightly backup".into(),
        left: r"C:\Users\me\Documents".into(),
        right: r"\\nas\backup\Documents".into(),
        facts: vec![
            Fact { label: "Mode".into(), value: "Mirror left \u{2192} right".into() },
            Fact { label: "Trigger".into(), value: "Daily at 02:00".into() },
            Fact { label: "Copied".into(), value: "12 files (48.3 MB)".into() },
            Fact { label: "Duration".into(), value: "1 min 12 s".into() },
        ],
        failures: vec![FailureLine {
            path: r"Reports\Q3 <draft> & notes.xlsx".into(),
            error: "The file is locked by another program (sample error)".into(),
        }],
        total_failures: 1,
        started_ms: now - 72_000,
        finished_ms: now,
        host: String::new(),
        app_version: env!("CARGO_PKG_VERSION").into(),
    }
}

// ---------- shared by the transports ----------

/// Attempts per delivery, including the first.
pub(crate) const ATTEMPTS: u32 = 3;
/// A server's `Retry-After` is honoured up to this long.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Failures listed by name in a notification; the rest are counted.
pub(crate) const MAX_LISTED_FAILURES: usize = 10;
/// Response bodies quoted in errors are cut to this many characters.
pub(crate) const MAX_ERROR_BODY: usize = 300;

/// The outcome of one delivery attempt that didn't succeed.
pub(crate) struct Failure {
    pub error: NotifyError,
    /// Worth another attempt (rate limit, server hiccup, network blip).
    pub transient: bool,
    /// The server asked us to wait this long first.
    pub retry_after: Option<Duration>,
}

impl Failure {
    pub fn permanent(error: NotifyError) -> Self {
        Failure {
            error,
            transient: false,
            retry_after: None,
        }
    }
    pub fn transient(error: NotifyError, retry_after: Option<Duration>) -> Self {
        Failure {
            error,
            transient: true,
            retry_after,
        }
    }
}

/// Runs `attempt` up to [`ATTEMPTS`] times while it fails transiently,
/// sleeping 1 s, then 2 s (doubling) in between, or what the server asked
/// for via `Retry-After` (capped at [`MAX_RETRY_AFTER`]).
pub(crate) fn with_retries<T>(
    mut attempt: impl FnMut() -> std::result::Result<T, Failure>,
) -> Result<T> {
    let mut n = 1;
    loop {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(f) if !f.transient || n >= ATTEMPTS => return Err(f.error),
            Err(f) => {
                let backoff = Duration::from_secs(1 << (n - 1));
                thread::sleep(f.retry_after.map_or(backoff, |d| d.min(MAX_RETRY_AFTER)));
                n += 1;
            }
        }
    }
}

/// `s` on one line, cut to `max` characters with an ellipsis, for quoting
/// service responses in error messages.
pub(crate) fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// The failures to list by name and how many more there were.
pub(crate) fn listed_failures(n: &Notification) -> (&[FailureLine], usize) {
    let shown = &n.failures[..n.failures.len().min(MAX_LISTED_FAILURES)];
    let total = n.total_failures.max(n.failures.len());
    (shown, total - shown.len())
}

/// The folders and facts in display order: Left and Right first (they
/// aren't in `facts`), skipping empty values.
pub(crate) fn all_facts(n: &Notification) -> Vec<(&str, &str)> {
    let mut out = Vec::with_capacity(n.facts.len() + 2);
    if !n.left.is_empty() {
        out.push(("Left", n.left.as_str()));
    }
    if !n.right.is_empty() {
        out.push(("Right", n.right.as_str()));
    }
    out.extend(
        n.facts
            .iter()
            .filter(|f| !f.value.is_empty())
            .map(|f| (f.label.as_str(), f.value.as_str())),
    );
    out
}

/// "2026-10-13 14:05 (UTC+02:00)" in the sending machine's time zone, so
/// readers elsewhere can still place it.
pub(crate) fn format_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%Y-%m-%d %H:%M (UTC%:z)").to_string()
        }
        chrono::LocalResult::None => String::new(),
    }
}

/// "host · PCompare 0.2.0 · finished 2026-10-13 14:05 (UTC+02:00)".
pub(crate) fn footer_text(n: &Notification) -> String {
    let mut parts = Vec::new();
    if !n.host.is_empty() {
        parts.push(n.host.clone());
    }
    parts.push(if n.app_version.is_empty() {
        "PCompare".to_string()
    } else {
        format!("PCompare {}", n.app_version)
    });
    if n.finished_ms > 0 {
        parts.push(format!("finished {}", format_time(n.finished_ms)));
    }
    parts.join(" \u{b7} ")
}

fn required_secret(
    secrets: &dyn SecretStore,
    profile: &str,
    key: &str,
    what: &'static str,
) -> Result<String> {
    optional_secret(secrets, profile, key)?.ok_or(NotifyError::MissingSecret(what))
}

fn optional_secret(secrets: &dyn SecretStore, profile: &str, key: &str) -> Result<Option<String>> {
    Ok(secrets
        .get(profile, key)?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Instant;

    #[test]
    fn retries_transient_failures_then_gives_up() {
        let calls = Cell::new(0);
        let start = Instant::now();
        let r: Result<()> = with_retries(|| {
            calls.set(calls.get() + 1);
            Err(Failure::transient(
                NotifyError::Transport("down".into()),
                Some(Duration::from_millis(10)),
            ))
        });
        assert!(matches!(r, Err(NotifyError::Transport(m)) if m == "down"));
        assert_eq!(calls.get(), 3);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "Retry-After overrides the backoff"
        );
    }

    #[test]
    fn permanent_failures_are_not_retried() {
        let calls = Cell::new(0);
        let r: Result<()> = with_retries(|| {
            calls.set(calls.get() + 1);
            Err(Failure::permanent(NotifyError::Config("bad".into())))
        });
        assert!(r.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn clip_flattens_and_truncates() {
        assert_eq!(clip("a\n  b\tc", 10), "a b c");
        let long = "x".repeat(400);
        let c = clip(&long, MAX_ERROR_BODY);
        assert_eq!(c.chars().count(), MAX_ERROR_BODY);
        assert!(c.ends_with('\u{2026}'));
    }

    #[test]
    fn listed_failures_caps_and_counts_the_rest() {
        let mut n = test_notification("x");
        n.failures = (0..12)
            .map(|i| FailureLine {
                path: format!("f{i}"),
                error: "e".into(),
            })
            .collect();
        n.total_failures = 40;
        let (shown, more) = listed_failures(&n);
        assert_eq!(shown.len(), 10);
        assert_eq!(more, 30);
        // A total lower than the list (caller slip) never underflows.
        n.total_failures = 0;
        assert_eq!(listed_failures(&n).1, 2);
    }

    #[test]
    fn test_notification_is_a_realistic_sample() {
        let n = test_notification("Ops team");
        assert_eq!(n.reason, NotifyReason::Test);
        assert_eq!(n.severity, Severity::Info);
        assert!(n.summary.contains("Ops team"));
        assert!(n.facts.len() >= 2);
        assert_eq!(n.failures.len(), 1);
        assert!(footer_text(&n).contains(&format!("PCompare {}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn send_reports_missing_secrets() {
        let secrets = pcompare_core::secrets::MemorySecrets::default();
        let n = test_notification("x");
        let mut ch = ChannelConfig {
            id: "c1".into(),
            name: "x".into(),
            enabled: true,
            settings: ChannelSettings::Teams {},
            created_ms: 0,
            updated_ms: 0,
        };
        assert!(matches!(
            send(&ch, &secrets, &n),
            Err(NotifyError::MissingSecret("webhook URL"))
        ));
        ch.settings = ChannelSettings::Email {
            host: "smtp.example.com".into(),
            port: 0,
            security: EmailSecurity::StartTls,
            username: "me".into(),
            from: "me@example.com".into(),
            to: vec!["you@example.com".into()],
            cc: vec![],
        };
        let err = send(&ch, &secrets, &n).unwrap_err();
        assert!(matches!(err, NotifyError::MissingSecret("password")));
        assert_eq!(err.to_string(), "the password for this channel isn't set");
    }
}
