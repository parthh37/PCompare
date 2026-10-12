//! SMTP transport: a multipart (plain text + HTML) message.

use std::error::Error as _;
use std::time::Duration;

use lettre::message::header::{Header, HeaderName, HeaderValue};
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{Message, SmtpTransport, Transport};

use crate::channel::EmailSecurity;
use crate::message::{Notification, Severity};
use crate::{
    all_facts, clip, footer_text, listed_failures, with_retries, Failure, NotifyError, Result,
    MAX_ERROR_BODY,
};

/// Applies to connecting and to every read/write on the connection.
pub const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How to reach and log in to the mail server.
pub struct SmtpAccount<'a> {
    pub host: &'a str,
    /// 0 = [`default_port`] for `security`.
    pub port: u16,
    pub security: EmailSecurity,
    /// Empty = no authentication.
    pub username: &'a str,
    pub password: Option<&'a str>,
}

pub fn default_port(security: EmailSecurity) -> u16 {
    match security {
        EmailSecurity::Tls => 465,
        EmailSecurity::StartTls => 587,
        EmailSecurity::None => 25,
    }
}

/// Checks the SMTP host is a bare server name, not a URL or an address.
pub(crate) fn validate_host(host: &str) -> Result<()> {
    let host = host.trim();
    if host.is_empty() {
        return Err(NotifyError::Config(
            "Enter the mail server (SMTP host), e.g. smtp.office365.com.".into(),
        ));
    }
    if host.contains("://")
        || host.contains('/')
        || host.contains('@')
        || host.contains(char::is_whitespace)
    {
        return Err(NotifyError::Config(format!(
            "\u{201c}{host}\u{201d} isn't a server name. Enter just the host, e.g. smtp.office365.com, and the port separately."
        )));
    }
    Ok(())
}

/// Parses `Name <address>` or a bare address; `what` names the field in
/// the error ("sender", "recipient").
pub(crate) fn parse_mailbox(what: &str, s: &str) -> Result<Mailbox> {
    s.trim().parse::<Mailbox>().map_err(|_| {
        NotifyError::Config(format!(
            "The {what} address \u{201c}{}\u{201d} isn't a valid email address.",
            s.trim()
        ))
    })
}

/// Checks the addresses of an email channel: a sender and at least one
/// recipient, all valid.
pub(crate) fn validate_addresses(
    from: &str,
    to: &[String],
    cc: &[String],
) -> Result<(Mailbox, Vec<Mailbox>, Vec<Mailbox>)> {
    if from.trim().is_empty() {
        return Err(NotifyError::Config(
            "Enter the sender (From) address.".into(),
        ));
    }
    let from = parse_mailbox("sender", from)?;
    let to = recipients("recipient", to)?;
    if to.is_empty() {
        return Err(NotifyError::Config(
            "Add at least one recipient (To) address.".into(),
        ));
    }
    let cc = recipients("Cc", cc)?;
    Ok((from, to, cc))
}

fn recipients(what: &str, list: &[String]) -> Result<Vec<Mailbox>> {
    list.iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| parse_mailbox(what, s))
        .collect()
}

/// `Auto-Submitted: auto-generated` (RFC 3834) keeps out-of-office replies
/// and auto-forward loops away from an unattended sender.
#[derive(Clone)]
struct AutoSubmitted;

impl Header for AutoSubmitted {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("Auto-Submitted")
    }
    fn parse(_: &str) -> std::result::Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(AutoSubmitted)
    }
    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), "auto-generated".to_string())
    }
}

/// The complete message: Subject = title, plain text and HTML alternatives.
pub fn build_message(
    from: &str,
    to: &[String],
    cc: &[String],
    n: &Notification,
) -> Result<Message> {
    let (from, to, cc) = validate_addresses(from, to, cc)?;
    let mut b = Message::builder()
        .from(from)
        .subject(n.title.clone())
        .header(AutoSubmitted);
    for m in to {
        b = b.to(m);
    }
    for m in cc {
        b = b.cc(m);
    }
    b.multipart(MultiPart::alternative_plain_html(
        render_text(n),
        render_html(n),
    ))
    .map_err(|e| NotifyError::Config(format!("Couldn't build the email: {e}")))
}

/// Sends `n` as an email, retrying transient failures (SMTP 4xx, timeouts,
/// dropped connections). Permanent replies (5xx, including authentication
/// failures) and TLS problems fail at once.
pub fn send_email(
    account: &SmtpAccount,
    from: &str,
    to: &[String],
    cc: &[String],
    n: &Notification,
) -> Result<()> {
    validate_host(account.host)?;
    let host = account.host.trim();
    let port = if account.port == 0 {
        default_port(account.security)
    } else {
        account.port
    };
    let message = build_message(from, to, cc, n)?;
    let tls = match account.security {
        EmailSecurity::None => Tls::None,
        sec => {
            let params = TlsParameters::new(host.to_string()).map_err(|e| {
                NotifyError::Transport(format!(
                    "Couldn't prepare a secure connection to {host}: {}",
                    root_cause(&e)
                ))
            })?;
            if sec == EmailSecurity::Tls {
                Tls::Wrapper(params)
            } else {
                Tls::Required(params)
            }
        }
    };
    let mut builder = SmtpTransport::builder_dangerous(host)
        .port(port)
        .tls(tls)
        .timeout(Some(SMTP_TIMEOUT));
    if let Some(name) = hello_name(&n.host) {
        builder = builder.hello_name(name);
    }
    if !account.username.trim().is_empty() {
        let password = account
            .password
            .ok_or(NotifyError::MissingSecret("password"))?;
        builder = builder.credentials(Credentials::new(
            account.username.trim().to_string(),
            password.to_string(),
        ));
    }
    let transport = builder.build();
    with_retries(|| {
        transport
            .send(&message)
            .map(|_| ())
            .map_err(|e| classify(&e, host, port, account.security))
    })
}

/// The machine name for EHLO when it is a plausible host name; otherwise
/// lettre's default (the `[127.0.0.1]` address literal).
fn hello_name(host: &str) -> Option<ClientId> {
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !host.starts_with(['-', '.']);
    ok.then(|| ClientId::Domain(host.to_string()))
}

fn classify(
    e: &lettre::transport::smtp::Error,
    host: &str,
    port: u16,
    security: EmailSecurity,
) -> Failure {
    if let Some(code) = e.status() {
        let status = code.to_string().parse().unwrap_or(0);
        let text = e.source().map(|s| s.to_string()).unwrap_or_default();
        let body = if text.trim().is_empty() {
            "no details".to_string()
        } else {
            clip(&text, MAX_ERROR_BODY)
        };
        let error = NotifyError::Rejected {
            service: "The mail server",
            status,
            body,
        };
        return if e.is_transient() {
            Failure::transient(error, None)
        } else {
            Failure::permanent(error)
        };
    }
    let cause = root_cause(e);
    if e.is_timeout() {
        return Failure::transient(
            NotifyError::Transport(format!(
                "The mail server {host}:{port} didn't answer within {} seconds.",
                SMTP_TIMEOUT.as_secs()
            )),
            None,
        );
    }
    let mismatch_hint = match security {
        EmailSecurity::None => "",
        _ => {
            " Check that the security setting matches the port (TLS is usually 465, STARTTLS 587)."
        }
    };
    if e.is_tls() {
        return Failure::permanent(NotifyError::Transport(format!(
            "Couldn't set up a secure connection to {host}:{port}: {cause}.{mismatch_hint}"
        )));
    }
    if e.is_client() || e.is_response() || e.is_transport_shutdown() {
        return Failure::permanent(NotifyError::Transport(format!(
            "The mail server {host}:{port} couldn't be used: {cause}.{mismatch_hint}"
        )));
    }
    // Connection refused, reset, DNS failure: possibly a blip, so retry.
    Failure::transient(
        NotifyError::Transport(format!(
            "Couldn't reach the mail server {host}:{port}: {cause}"
        )),
        None,
    )
}

fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

// ---------- rendering ----------

/// The plain-text alternative: aligned facts, failures indented.
pub fn render_text(n: &Notification) -> String {
    let mut out = format!("{}\n\n{}\n", n.title, n.summary);
    let facts = all_facts(n);
    if !facts.is_empty() {
        out.push('\n');
        let width = facts
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0)
            + 1;
        for (label, value) in &facts {
            out.push_str(&format!("{:<width$} {value}\n", format!("{label}:")));
        }
    }
    let (shown, more) = listed_failures(n);
    if !shown.is_empty() || more > 0 {
        let total = shown.len() + more;
        out.push_str(&format!(
            "\n{}:\n",
            if total == 1 {
                "1 failure".to_string()
            } else {
                format!("{total} failures")
            }
        ));
        for f in shown {
            out.push_str(&format!("  {} \u{2014} {}\n", f.path, f.error));
        }
        if more > 0 {
            out.push_str(&format!("  and {more} more\n"));
        }
    }
    out.push_str(&format!("\n--\n{}\n", footer_text(n)));
    out
}

/// Escapes text for HTML element content and attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn accent(s: Severity) -> &'static str {
    match s {
        Severity::Error => "#c4314b",
        Severity::Warning => "#b7791f",
        Severity::Success => "#237b4b",
        Severity::Info => "#2f6fde",
    }
}

/// The HTML alternative: a single card with inline styles only (mail
/// clients drop `<style>` blocks), every piece of text escaped.
pub fn render_html(n: &Notification) -> String {
    const FONT: &str = "font-family:'Segoe UI',Helvetica,Arial,sans-serif;";
    let color = accent(n.severity);
    let mut h = String::with_capacity(4096);
    h.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head>");
    h.push_str("<body style=\"margin:0;padding:0;background:#f4f5f7;\">");
    h.push_str(&format!(
        "<div style=\"{FONT}max-width:680px;margin:0 auto;padding:16px;\">\
         <div style=\"background:#ffffff;border:1px solid #dde1e6;border-top:4px solid {color};border-radius:6px;padding:20px;\">"
    ));
    h.push_str(&format!(
        "<h1 style=\"margin:0 0 8px;font-size:20px;line-height:1.3;color:{color};\">{}</h1>",
        html_escape(&n.title)
    ));
    h.push_str(&format!(
        "<p style=\"margin:0 0 16px;font-size:14px;line-height:1.5;color:#1f2328;\">{}</p>",
        html_escape(&n.summary)
    ));
    let facts = all_facts(n);
    if !facts.is_empty() {
        h.push_str("<table role=\"presentation\" style=\"border-collapse:collapse;font-size:13px;margin:0 0 16px;\">");
        for (label, value) in facts {
            h.push_str(&format!(
                "<tr><td style=\"padding:3px 16px 3px 0;color:#59636e;white-space:nowrap;vertical-align:top;\">{}</td>\
                 <td style=\"padding:3px 0;color:#1f2328;word-break:break-word;\">{}</td></tr>",
                html_escape(label),
                html_escape(value)
            ));
        }
        h.push_str("</table>");
    }
    let (shown, more) = listed_failures(n);
    if !shown.is_empty() || more > 0 {
        let total = shown.len() + more;
        h.push_str(&format!(
            "<h2 style=\"margin:0 0 8px;font-size:15px;color:#1f2328;\">{}</h2>",
            if total == 1 {
                "1 failure".to_string()
            } else {
                format!("{total} failures")
            }
        ));
        h.push_str("<ul style=\"margin:0 0 16px;padding-left:20px;font-size:13px;line-height:1.5;color:#1f2328;\">");
        for f in shown {
            h.push_str(&format!(
                "<li><code style=\"font-family:Consolas,Menlo,monospace;background:#f4f5f7;padding:1px 4px;border-radius:3px;\">{}</code> \u{2014} {}</li>",
                html_escape(&f.path),
                html_escape(&f.error)
            ));
        }
        if more > 0 {
            h.push_str(&format!(
                "<li style=\"color:#59636e;\">and {more} more</li>"
            ));
        }
        h.push_str("</ul>");
    }
    h.push_str(&format!(
        "<p style=\"margin:16px 0 0;padding-top:12px;border-top:1px solid #dde1e6;font-size:12px;color:#59636e;\">{}</p>",
        html_escape(&footer_text(n))
    ));
    h.push_str("</div></div></body></html>");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FailureLine, NotifyReason};
    use crate::test_notification;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn sample() -> Notification {
        let mut n = test_notification("Mail");
        n.reason = NotifyReason::Failure;
        n.severity = Severity::Error;
        n.title = "Sync failed: <Nightly> & \"backup\"".into();
        n.host = "BUILD-01".into();
        n
    }

    #[test]
    fn html_escapes_everything() {
        let mut n = sample();
        n.summary = "<script>alert('x')</script>".into();
        n.facts.push(crate::Fact {
            label: "<b>".into(),
            value: "a&b".into(),
        });
        n.failures = vec![FailureLine {
            path: "<img src=x onerror=1>".into(),
            error: "\"quoted\"".into(),
        }];
        let h = render_html(&n);
        assert!(!h.contains("<script>") && !h.contains("<img") && !h.contains("<b>"));
        assert!(h.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
        assert!(h.contains("Sync failed: &lt;Nightly&gt; &amp; &quot;backup&quot;"));
        assert!(h.contains("&lt;img src=x onerror=1&gt;"));
        assert!(h.contains("a&amp;b"));
        assert!(h.contains("#c4314b"), "error accent colour");
    }

    #[test]
    fn text_lists_facts_and_failures() {
        let mut n = sample();
        n.failures = (0..11)
            .map(|i| FailureLine {
                path: format!("f{i}"),
                error: "denied".into(),
            })
            .collect();
        n.total_failures = 11;
        let t = render_text(&n);
        assert!(t.starts_with("Sync failed: <Nightly> & \"backup\"\n\n"));
        assert!(t.contains("Left:     C:\\Users\\me\\Documents\n"), "{t}");
        assert!(t.contains("Duration: 1 min 12 s\n"), "{t}");
        assert!(t.contains("\n11 failures:\n  f0 \u{2014} denied\n"));
        assert!(t.contains("  and 1 more\n"));
        assert!(t.contains("BUILD-01 \u{b7} PCompare"));
    }

    #[test]
    fn address_validation() {
        assert!(
            validate_addresses("PCompare <pc@example.com>", &["a@example.com".into()], &[]).is_ok()
        );
        let e = validate_addresses("not an address", &["a@example.com".into()], &[])
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "The sender address \u{201c}not an address\u{201d} isn't a valid email address."
        );
        assert!(validate_addresses("pc@example.com", &[" ".into()], &[])
            .unwrap_err()
            .to_string()
            .contains("at least one recipient"));
        assert!(validate_addresses(
            "pc@example.com",
            &["a@example.com".into()],
            &["oops".into()]
        )
        .unwrap_err()
        .to_string()
        .contains("Cc"));
        assert!(validate_addresses("", &["a@example.com".into()], &[]).is_err());
        assert!(validate_host("smtp.example.com").is_ok());
        assert!(validate_host("smtp://smtp.example.com").is_err());
        assert!(validate_host(" ").is_err());
    }

    #[test]
    fn default_ports() {
        assert_eq!(default_port(EmailSecurity::Tls), 465);
        assert_eq!(default_port(EmailSecurity::StartTls), 587);
        assert_eq!(default_port(EmailSecurity::None), 25);
    }

    #[test]
    fn message_is_multipart_alternative() {
        let m = build_message(
            "PCompare <pc@example.com>",
            &["ops@example.com".into()],
            &["boss@example.com".into()],
            &sample(),
        )
        .unwrap();
        let raw = String::from_utf8(m.formatted()).unwrap();
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("text/plain"));
        assert!(raw.contains("text/html"));
        assert!(raw.contains("Auto-Submitted: auto-generated"));
        assert!(raw.contains("Cc: boss@example.com"));
    }

    /// What the fake server saw, and how it should answer `MAIL FROM`.
    #[derive(Default)]
    struct SmtpLog {
        commands: Vec<String>,
        data: Vec<String>,
        connections: usize,
    }

    /// A minimal plain-text SMTP server: `mail_replies` scripts the answer
    /// to MAIL FROM per connection (e.g. a 451 then a 250).
    fn fake_smtp(mail_replies: Vec<&'static str>) -> (u16, Arc<Mutex<SmtpLog>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(SmtpLog::default()));
        let log2 = Arc::clone(&log);
        thread::spawn(move || {
            for mail_reply in mail_replies {
                let (stream, _) = listener.accept().unwrap();
                log2.lock().unwrap().connections += 1;
                session(stream, mail_reply, &log2);
            }
        });
        (port, log)
    }

    fn session(stream: TcpStream, mail_reply: &str, log: &Mutex<SmtpLog>) {
        let mut out = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut say = |s: &str| {
            let _ = out.write_all(format!("{s}\r\n").as_bytes());
        };
        say("220 fake.local ESMTP ready");
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let cmd = line.trim_end().to_string();
            log.lock().unwrap().commands.push(cmd.clone());
            let upper = cmd.to_ascii_uppercase();
            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                say("250-fake.local\r\n250 8BITMIME");
            } else if upper.starts_with("MAIL FROM") {
                say(mail_reply);
            } else if upper.starts_with("DATA") {
                say("354 go ahead");
                let mut data = String::new();
                loop {
                    let mut l = String::new();
                    if reader.read_line(&mut l).unwrap_or(0) == 0 || l == ".\r\n" {
                        break;
                    }
                    data.push_str(&l);
                }
                log.lock().unwrap().data.push(data);
                say("250 queued");
            } else if upper.starts_with("QUIT") {
                say("221 bye");
                return;
            } else {
                say("250 ok");
            }
        }
    }

    fn account(port: u16) -> SmtpAccount<'static> {
        SmtpAccount {
            host: "127.0.0.1",
            port,
            security: EmailSecurity::None,
            username: "",
            password: None,
        }
    }

    #[test]
    fn delivers_over_plain_smtp() {
        let (port, log) = fake_smtp(vec!["250 ok"]);
        send_email(
            &account(port),
            "PCompare <pc@example.com>",
            &["ops@example.com".into()],
            &["cc@example.com".into()],
            &sample(),
        )
        .unwrap();
        let log = log.lock().unwrap();
        assert!(
            log.commands.iter().any(|c| c == "EHLO BUILD-01"),
            "{:?}",
            log.commands
        );
        assert!(log
            .commands
            .iter()
            .any(|c| c.starts_with("MAIL FROM:<pc@example.com>")));
        assert!(log
            .commands
            .iter()
            .any(|c| c == "RCPT TO:<ops@example.com>"));
        assert!(log.commands.iter().any(|c| c == "RCPT TO:<cc@example.com>"));
        let data = &log.data[0];
        assert!(data.contains("multipart/alternative"));
        assert!(data.contains("Content-Type: text/plain"));
        assert!(data.contains("Content-Type: text/html"));
        assert!(data.contains("Subject: "));
    }

    #[test]
    fn retries_transient_smtp_replies() {
        let (port, log) = fake_smtp(vec!["451 4.3.0 try again later", "250 ok"]);
        send_email(
            &account(port),
            "pc@example.com",
            &["ops@example.com".into()],
            &[],
            &sample(),
        )
        .unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.connections, 2);
        assert_eq!(log.data.len(), 1);
    }

    #[test]
    fn permanent_smtp_replies_fail_at_once() {
        let (port, log) = fake_smtp(vec!["550 5.7.1 relaying denied", "250 ok"]);
        let err = send_email(
            &account(port),
            "pc@example.com",
            &["ops@example.com".into()],
            &[],
            &sample(),
        )
        .unwrap_err();
        assert!(
            matches!(&err, NotifyError::Rejected { service: "The mail server", status: 550, body } if body.contains("relaying denied")),
            "{err:?}"
        );
        assert_eq!(log.lock().unwrap().connections, 1);
    }

    #[test]
    fn username_without_password_is_a_missing_secret() {
        let acct = SmtpAccount {
            username: "me",
            ..account(1)
        };
        let err = send_email(
            &acct,
            "pc@example.com",
            &["ops@example.com".into()],
            &[],
            &sample(),
        )
        .unwrap_err();
        assert!(matches!(err, NotifyError::MissingSecret("password")));
    }

    #[test]
    fn hello_name_only_for_plausible_hosts() {
        assert!(matches!(hello_name("DESKTOP-01"), Some(ClientId::Domain(d)) if d == "DESKTOP-01"));
        assert!(hello_name("").is_none());
        assert!(hello_name("my pc").is_none());
    }
}
