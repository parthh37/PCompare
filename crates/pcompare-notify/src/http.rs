//! Webhook transports: Teams (Adaptive Card), Slack (Block Kit) and the
//! generic signed JSON webhook.
//!
//! Webhook URLs are credentials (anyone holding one can post), so they never
//! appear in errors: messages name the host at most.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use reqwest::Url;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::message::{Notification, Severity};
use crate::{
    all_facts, clip, footer_text, listed_failures, with_retries, Failure, NotifyError, Result,
    MAX_ERROR_BODY,
};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Header carrying `sha256=<hex HMAC-SHA256 of the raw body>`.
pub const SIGNATURE_HEADER: &str = "x-pcompare-signature";
/// Header carrying the send time in Unix seconds (informational; the
/// signature covers the body only, which already holds the run's times).
pub const TIMESTAMP_HEADER: &str = "x-pcompare-timestamp";

/// Headers a user can't set on a webhook: we own the framing, the content
/// type and the signature.
const RESERVED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "host",
    "transfer-encoding",
    "connection",
    SIGNATURE_HEADER,
    TIMESTAMP_HEADER,
];

/// Checks a webhook URL: it must be `https://`, except plain `http://` to
/// this machine (`localhost`, `127.0.0.1`, `[::1]`) for testing. The URL
/// itself is never quoted back because it usually embeds a token.
pub fn validate_url(url: &str) -> Result<Url> {
    let parsed = Url::parse(url.trim())
        .map_err(|_| NotifyError::Config("The webhook URL isn't a valid web address. Paste the full URL, starting with https://.".into()))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match parsed.scheme() {
        "https" if parsed.host_str().is_some_and(|h| !h.is_empty()) => Ok(parsed),
        "http" if local => Ok(parsed),
        "http" => Err(NotifyError::Config(
            "The webhook URL must start with https:// so the notification is encrypted in transit (plain http is only allowed for localhost)."
                .into(),
        )),
        _ => Err(NotifyError::Config("The webhook URL must start with https://.".into())),
    }
}

/// Checks a webhook's extra headers and turns them into a header map.
/// Rows with neither a name nor a value (blank rows in the editor) are skipped.
pub fn validate_headers(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() && value.is_empty() {
            continue;
        }
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            NotifyError::Config(format!(
                "\u{201c}{name}\u{201d} isn't a valid HTTP header name."
            ))
        })?;
        if RESERVED_HEADERS.contains(&header.as_str()) {
            return Err(NotifyError::Config(format!(
                "The {name} header is set by PCompare and can't be overridden."
            )));
        }
        let value = HeaderValue::from_str(value).map_err(|_| {
            NotifyError::Config(format!(
                "The value of the {name} header contains characters HTTP doesn't allow."
            ))
        })?;
        map.append(header, value);
    }
    Ok(map)
}

// ---------- senders ----------

pub fn send_teams(url: &str, n: &Notification) -> Result<()> {
    let url = validate_url(url)?;
    post(
        "Microsoft Teams",
        &url,
        &n.app_version,
        HeaderMap::new(),
        body_of(&teams_payload(n)),
        teams_reply,
    )
}

pub fn send_slack(url: &str, n: &Notification) -> Result<()> {
    let url = validate_url(url)?;
    post(
        "Slack",
        &url,
        &n.app_version,
        HeaderMap::new(),
        body_of(&slack_payload(n)),
        slack_reply,
    )
}

/// POSTs [`webhook_payload`]; with `signing_key`, adds
/// `X-PCompare-Signature: sha256=<hex HMAC-SHA256 of the body>` and
/// `X-PCompare-Timestamp: <unix seconds>`.
pub fn send_webhook(
    url: &str,
    headers: &[(String, String)],
    signing_key: Option<&str>,
    n: &Notification,
) -> Result<()> {
    let url = validate_url(url)?;
    let mut map = validate_headers(headers)?;
    let body = body_of(&webhook_payload(n));
    if let Some(key) = signing_key.filter(|k| !k.is_empty()) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        map.insert(
            SIGNATURE_HEADER,
            HeaderValue::from_str(&signature(key, &body)).expect("hex is a valid header value"),
        );
        map.insert(TIMESTAMP_HEADER, HeaderValue::from(secs));
    }
    post("The webhook", &url, &n.app_version, map, body, |_| Ok(()))
}

/// `sha256=<lowercase hex>` of HMAC-SHA256(key, body).
pub fn signature(key: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256=");
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn body_of(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("a JSON value always serializes")
}

/// Legacy Office 365 connectors answer 200 with the body `1` on success,
/// but also report some failures (throttling, bad cards) as 200 with an
/// error sentence; Workflows webhooks answer 202 with an empty or JSON body.
/// So a 2xx is only trusted when the body is empty, `1` or JSON — anything
/// else (an error sentence, or a proxy's sign-in page) is reported as a
/// failure with the text quoted, which is more useful than a silent loss.
fn teams_reply(body: &str) -> std::result::Result<(), Failure> {
    let t = body.trim();
    if t.is_empty() || t == "1" || t.starts_with('{') || t.starts_with('[') {
        return Ok(());
    }
    let error = NotifyError::Rejected {
        service: "Microsoft Teams",
        status: 200,
        body: clip(t, MAX_ERROR_BODY),
    };
    Err(if t.contains("429") {
        Failure::transient(error, None)
    } else {
        Failure::permanent(error)
    })
}

/// Slack answers exactly `ok`; any other 2xx body is most likely a proxy page.
fn slack_reply(body: &str) -> std::result::Result<(), Failure> {
    let t = body.trim();
    if t.is_empty() || t == "ok" {
        Ok(())
    } else {
        Err(Failure::permanent(NotifyError::Rejected {
            service: "Slack",
            status: 200,
            body: clip(t, MAX_ERROR_BODY),
        }))
    }
}

/// POSTs a JSON body with retries. Redirects are not followed: a webhook
/// that moved should be re-pasted, and following one could forward a signed
/// payload (or downgrade to http) without the user knowing.
fn post(
    service: &'static str,
    url: &Url,
    app_version: &str,
    headers: HeaderMap,
    body: Vec<u8>,
    check_2xx: impl Fn(&str) -> std::result::Result<(), Failure>,
) -> Result<()> {
    let client = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(if app_version.is_empty() {
            "PCompare".to_string()
        } else {
            format!("PCompare/{app_version}")
        })
        .build()
        .map_err(|e| {
            NotifyError::Transport(format!(
                "Couldn't set up the HTTP client: {}",
                root_cause(&e)
            ))
        })?;
    let host = url.host_str().unwrap_or("the server").to_string();
    with_retries(|| {
        let resp = client
            .post(url.clone())
            .headers(headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body.clone())
            .send()
            .map_err(|e| request_failure(&host, e))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = resp.text().unwrap_or_default();
        if status.is_success() {
            return check_2xx(&text);
        }
        let body = if text.trim().is_empty() {
            status
                .canonical_reason()
                .unwrap_or("no details")
                .to_string()
        } else {
            clip(&text, MAX_ERROR_BODY)
        };
        let error = NotifyError::Rejected {
            service,
            status: status.as_u16(),
            body,
        };
        let code = status.as_u16();
        if code == 408 || code == 429 || status.is_server_error() {
            Err(Failure::transient(error, retry_after))
        } else {
            Err(Failure::permanent(error))
        }
    })
}

/// A request that got no HTTP answer at all. Connection and timeout
/// problems are worth retrying; anything else (a malformed request) isn't.
fn request_failure(host: &str, e: reqwest::Error) -> Failure {
    let transient = e.is_timeout() || e.is_connect() || e.is_request();
    // Error texts otherwise quote the URL, and with it the webhook's token.
    let e = e.without_url();
    let message = if e.is_timeout() {
        format!(
            "{host} didn't answer within {} seconds.",
            REQUEST_TIMEOUT.as_secs()
        )
    } else {
        format!("Couldn't reach {host}: {}", root_cause(&e))
    };
    let error = NotifyError::Transport(message);
    if transient {
        Failure::transient(error, None)
    } else {
        Failure::permanent(error)
    }
}

/// The innermost error message ("Connection refused (os error 61)",
/// "invalid peer certificate: UnknownIssuer"), which is the useful part;
/// the outer layers only say "error sending request" plus the secret URL.
fn root_cause(e: &reqwest::Error) -> String {
    let mut cur: &dyn std::error::Error = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

// ---------- payloads ----------

fn teams_color(s: Severity) -> &'static str {
    match s {
        Severity::Error => "attention",
        Severity::Warning => "warning",
        Severity::Success => "good",
        Severity::Info => "accent",
    }
}

/// The Teams message: one Adaptive Card 1.4 attachment, which both Teams
/// Workflows webhooks and legacy connectors accept.
///
/// Free text (summary, file paths, errors) goes in `RichTextBlock` text
/// runs, which Teams shows verbatim; `TextBlock`/`FactSet` text is parsed as
/// markdown, which would eat the backslashes of `\\server\share` paths.
pub fn teams_payload(n: &Notification) -> Value {
    let mut body = vec![
        json!({
            "type": "TextBlock",
            "text": n.title,
            "size": "Large",
            "weight": "Bolder",
            "color": teams_color(n.severity),
            "wrap": true,
        }),
        rich_text(vec![text_run(&n.summary)], "Small"),
    ];
    let facts = all_facts(n);
    if !facts.is_empty() {
        // FactSet values are markdown too, so each row is a pair of columns
        // holding verbatim text runs (labels are ours, values may be paths).
        let rows: Vec<Value> = facts
            .iter()
            .map(|(label, value)| {
                json!({
                    "type": "ColumnSet",
                    "spacing": "Small",
                    "columns": [
                        {"type": "Column", "width": "120px", "items": [rich_text(vec![json!({"type": "TextRun", "text": label, "weight": "Bolder"})], "None")]},
                        {"type": "Column", "width": "stretch", "items": [rich_text(vec![text_run(value)], "None")]},
                    ],
                })
            })
            .collect();
        body.push(json!({"type": "Container", "spacing": "Medium", "items": rows}));
    }
    let (shown, more) = listed_failures(n);
    if !shown.is_empty() || more > 0 {
        let total = shown.len() + more;
        body.push(json!({
            "type": "TextBlock",
            "text": if total == 1 { "1 failure".to_string() } else { format!("{total} failures") },
            "weight": "Bolder",
            "spacing": "Medium",
            "separator": true,
            "wrap": true,
        }));
        for f in shown {
            body.push(rich_text(
                vec![
                    json!({"type": "TextRun", "text": f.path, "fontType": "Monospace"}),
                    text_run(&format!(" \u{2014} {}", f.error)),
                ],
                "Small",
            ));
        }
        if more > 0 {
            body.push(rich_text(vec![json!({"type": "TextRun", "text": format!("and {more} more"), "isSubtle": true})], "Small"));
        }
    }
    body.push(json!({
        "type": "TextBlock",
        "text": footer_text(n),
        "size": "Small",
        "isSubtle": true,
        "wrap": true,
        "spacing": "Medium",
        "separator": true,
    }));
    json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": {
                "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                "type": "AdaptiveCard",
                "version": "1.4",
                "msteams": {"width": "Full"},
                "body": body,
            },
        }],
    })
}

fn text_run(text: &str) -> Value {
    json!({"type": "TextRun", "text": text})
}

fn rich_text(inlines: Vec<Value>, spacing: &str) -> Value {
    json!({"type": "RichTextBlock", "spacing": spacing, "inlines": inlines})
}

/// Slack's mrkdwn needs only `&`, `<` and `>` escaped.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Cuts to Slack's per-field character limits.
fn slack_clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('\u{2026}');
        out
    }
}

fn slack_icon(s: Severity) -> &'static str {
    match s {
        Severity::Error => ":x:",
        Severity::Warning => ":warning:",
        Severity::Success => ":white_check_mark:",
        Severity::Info => ":information_source:",
    }
}

/// The Slack message: Block Kit (header, summary, up to 10 fact fields,
/// failures in a code block, a context footer) plus the `text` fallback
/// used in notifications and by clients that can't show blocks.
pub fn slack_payload(n: &Notification) -> Value {
    let mut blocks = vec![
        json!({
            "type": "header",
            "text": {"type": "plain_text", "text": slack_clip(&format!("{} {}", slack_icon(n.severity), n.title), 150), "emoji": true},
        }),
        json!({"type": "section", "text": {"type": "mrkdwn", "text": slack_clip(&slack_escape(&n.summary), 3000)}}),
    ];
    let fields: Vec<Value> = all_facts(n)
        .iter()
        .take(10)
        .map(|(label, value)| {
            json!({"type": "mrkdwn", "text": slack_clip(&format!("*{}*\n{}", slack_escape(label), slack_escape(value)), 2000)})
        })
        .collect();
    if !fields.is_empty() {
        blocks.push(json!({"type": "section", "fields": fields}));
    }
    let (shown, more) = listed_failures(n);
    if !shown.is_empty() || more > 0 {
        let total = shown.len() + more;
        let mut text = if total == 1 {
            "*1 failure*".to_string()
        } else {
            format!("*{total} failures*")
        };
        if !shown.is_empty() {
            // Inside a code block nothing is formatted; only a literal fence
            // could end it early, so those are defused.
            let lines: Vec<String> = shown
                .iter()
                .map(|f| {
                    slack_clip(
                        &slack_escape(&format!("{} \u{2014} {}", f.path, f.error))
                            .replace("```", "'''"),
                        260,
                    )
                })
                .collect();
            text.push_str(&format!("\n```{}```", lines.join("\n")));
        }
        if more > 0 {
            text.push_str(&format!("\n_and {more} more_"));
        }
        blocks.push(
            json!({"type": "section", "text": {"type": "mrkdwn", "text": slack_clip(&text, 3000)}}),
        );
    }
    blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": slack_clip(&slack_footer(n), 3000)}]}));
    json!({
        "text": slack_clip(&slack_escape(&format!("{}: {}", n.title, n.summary)), 3000),
        "blocks": blocks,
    })
}

/// Like [`footer_text`], but the finish time uses Slack's date token so
/// each reader sees it in their own time zone (the fallback is ours).
fn slack_footer(n: &Notification) -> String {
    let mut parts = Vec::new();
    if !n.host.is_empty() {
        parts.push(slack_escape(&n.host));
    }
    parts.push(if n.app_version.is_empty() {
        "PCompare".to_string()
    } else {
        format!("PCompare {}", slack_escape(&n.app_version))
    });
    if n.finished_ms > 0 {
        let fallback = crate::format_time(n.finished_ms);
        parts.push(format!(
            "finished <!date^{}^{{date_short_pretty}} at {{time}}|{}>",
            n.finished_ms / 1000,
            fallback
        ));
    }
    parts.join(" \u{b7} ")
}

/// The generic webhook body: the [`Notification`] as JSON plus
/// `"event": "pcompare.sync.<reason>"` and `"version": 1`, so receivers can
/// route on the event and detect future format changes.
pub fn webhook_payload(n: &Notification) -> Value {
    let mut v = serde_json::to_value(n).expect("a notification always serializes");
    let reason = serde_json::to_value(n.reason).expect("a reason always serializes");
    if let Value::Object(map) = &mut v {
        map.insert(
            "event".into(),
            Value::String(format!(
                "pcompare.sync.{}",
                reason.as_str().unwrap_or("unknown")
            )),
        );
        map.insert("version".into(), json!(1));
    }
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::message::{FailureLine, NotifyReason};
    use crate::test_notification;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    pub(crate) struct Recorded {
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Recorded {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
        pub fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    /// One scripted reply: status, extra headers, body.
    type Reply = (u16, Vec<(&'static str, &'static str)>, &'static str);

    /// A tiny HTTP/1.1 server on 127.0.0.1 that answers each connection with
    /// the next scripted reply (closing it, so every attempt is a new
    /// connection) and records what it received.
    fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        thread::spawn(move || {
            for (status, headers, body) in replies {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let mut req_headers = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        req_headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                }
                let len: usize = req_headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .map_or(0, |(_, v)| v.parse().unwrap());
                let mut req_body = vec![0; len];
                reader.read_exact(&mut req_body).unwrap();
                log2.lock().unwrap().push(Recorded {
                    path,
                    headers: req_headers,
                    body: req_body,
                });
                let mut out = stream;
                let mut resp = format!(
                    "HTTP/1.1 {status} Scripted\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(body);
                let _ = out.write_all(resp.as_bytes());
            }
        });
        (base, log)
    }

    fn failing() -> Notification {
        let mut n = test_notification("Ops");
        n.reason = NotifyReason::Failure;
        n.severity = Severity::Error;
        n.title = "Sync failed: Nightly *backup*".into();
        n.host = "BUILD-01".into();
        n.failures = (0..12)
            .map(|i| FailureLine {
                path: format!(r"\\nas\share\f_{i}.txt"),
                error: "Access is denied".into(),
            })
            .collect();
        n.total_failures = 15;
        n
    }

    #[test]
    fn teams_card_shape() {
        let v = teams_payload(&failing());
        assert_eq!(v["type"], "message");
        let att = &v["attachments"][0];
        assert_eq!(
            att["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        assert!(att["contentUrl"].is_null());
        let card = &att["content"];
        assert_eq!(card["type"], "AdaptiveCard");
        assert_eq!(card["version"], "1.4");
        assert_eq!(
            card["$schema"],
            "http://adaptivecards.io/schemas/adaptive-card.json"
        );
        assert_eq!(card["msteams"]["width"], "Full");
        let body = card["body"].as_array().unwrap();
        assert_eq!(body[0]["text"], "Sync failed: Nightly *backup*");
        assert_eq!(body[0]["color"], "attention");
        assert_eq!(body[1]["inlines"][0]["text"], failing().summary);
        // Left, Right, then the 4 facts, verbatim.
        let rows = body[2]["items"].as_array().unwrap();
        assert_eq!(rows.len(), 6);
        assert_eq!(
            rows[1]["columns"][0]["items"][0]["inlines"][0]["text"],
            "Right"
        );
        assert_eq!(
            rows[1]["columns"][1]["items"][0]["inlines"][0]["text"],
            r"\\nas\backup\Documents"
        );
        assert_eq!(body[3]["text"], "15 failures");
        let lines: Vec<&Value> = body
            .iter()
            .filter(|b| b["type"] == "RichTextBlock")
            .skip(1)
            .collect();
        assert_eq!(lines.len(), 11, "10 failures + 'and N more'");
        assert_eq!(lines[0]["inlines"][0]["text"], r"\\nas\share\f_0.txt");
        assert_eq!(lines[0]["inlines"][1]["text"], " \u{2014} Access is denied");
        assert_eq!(lines[10]["inlines"][0]["text"], "and 5 more");
        let footer = body.last().unwrap()["text"].as_str().unwrap();
        assert!(footer.starts_with("BUILD-01 \u{b7} PCompare "), "{footer}");
        assert!(footer.contains("finished "));
    }

    #[test]
    fn teams_colors_follow_severity() {
        let mut n = test_notification("x");
        for (s, c) in [
            (Severity::Info, "accent"),
            (Severity::Success, "good"),
            (Severity::Warning, "warning"),
        ] {
            n.severity = s;
            assert_eq!(
                teams_payload(&n)["attachments"][0]["content"]["body"][0]["color"],
                c
            );
        }
    }

    #[test]
    fn slack_blocks_shape_and_escaping() {
        let mut n = failing();
        n.summary = "a < b & c > d".into();
        let v = slack_payload(&n);
        assert!(v["text"]
            .as_str()
            .unwrap()
            .starts_with("Sync failed: Nightly *backup*: a &lt; b &amp; c &gt; d"));
        let blocks = v["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(
            blocks[0]["text"]["text"],
            ":x: Sync failed: Nightly *backup*"
        );
        assert_eq!(blocks[1]["text"]["text"], "a &lt; b &amp; c &gt; d");
        let fields = blocks[2]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0]["text"], "*Left*\nC:\\Users\\me\\Documents");
        let failures = blocks[3]["text"]["text"].as_str().unwrap();
        assert!(failures
            .starts_with("*15 failures*\n```\\\\nas\\share\\f_0.txt \u{2014} Access is denied\n"));
        assert!(failures.ends_with("```\n_and 5 more_"));
        assert_eq!(blocks[4]["type"], "context");
        let footer = blocks[4]["elements"][0]["text"].as_str().unwrap();
        assert!(footer.contains("<!date^"), "{footer}");
    }

    #[test]
    fn slack_caps_fields_at_ten() {
        let mut n = test_notification("x");
        n.facts = (0..15)
            .map(|i| crate::Fact {
                label: format!("L{i}"),
                value: "v".into(),
            })
            .collect();
        let v = slack_payload(&n);
        assert_eq!(v["blocks"][2]["fields"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn webhook_payload_adds_event_and_version() {
        let v = webhook_payload(&failing());
        assert_eq!(v["event"], "pcompare.sync.failure");
        assert_eq!(v["version"], 1);
        assert_eq!(v["taskName"], "Example: Nightly backup");
        assert_eq!(v["totalFailures"], 15);
        assert_eq!(
            v["failures"].as_array().unwrap().len(),
            12,
            "the raw payload is not capped"
        );
        assert_eq!(
            webhook_payload(&test_notification("x"))["event"],
            "pcompare.sync.test"
        );
    }

    #[test]
    fn signature_matches_known_vector() {
        // RFC 4231 test case 2.
        assert_eq!(
            signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn url_validation() {
        assert!(validate_url("https://example.webhook.office.com/webhookb2/x").is_ok());
        assert!(validate_url("  https://hooks.slack.com/services/T/B/X  ").is_ok());
        assert!(validate_url("http://localhost:8080/hook").is_ok());
        assert!(validate_url("http://127.0.0.1/hook").is_ok());
        assert!(validate_url("http://[::1]:9/hook").is_ok());
        let e = validate_url("http://example.com/hook?token=SECRET")
            .unwrap_err()
            .to_string();
        assert!(e.contains("https://") && !e.contains("SECRET"), "{e}");
        assert!(matches!(
            validate_url("ftp://example.com"),
            Err(NotifyError::Config(_))
        ));
        assert!(
            matches!(validate_url("not a url SECRET"), Err(NotifyError::Config(m)) if !m.contains("SECRET"))
        );
    }

    #[test]
    fn header_validation() {
        let ok = validate_headers(&[
            ("X-Api-Key".into(), "abc".into()),
            (String::new(), String::new()),
        ])
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert!(matches!(
            validate_headers(&[("Content-Type".into(), "text/plain".into())]),
            Err(NotifyError::Config(_))
        ));
        assert!(matches!(
            validate_headers(&[("X-PCompare-Signature".into(), "x".into())]),
            Err(NotifyError::Config(_))
        ));
        assert!(matches!(
            validate_headers(&[("bad name".into(), "x".into())]),
            Err(NotifyError::Config(_))
        ));
        assert!(matches!(
            validate_headers(&[("X-Ok".into(), "line\nbreak".into())]),
            Err(NotifyError::Config(_))
        ));
    }

    #[test]
    fn teams_accepts_workflows_202() {
        let (base, log) = serve(vec![(202, vec![], "")]);
        send_teams(&format!("{base}/workflows/abc"), &failing()).unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].path, "/workflows/abc");
        assert_eq!(log[0].header("content-type"), Some("application/json"));
        assert_eq!(
            log[0].json()["attachments"][0]["content"]["type"],
            "AdaptiveCard"
        );
    }

    #[test]
    fn teams_legacy_connector_replies() {
        let (base, log) = serve(vec![
            (200, vec![], "1"),
            (200, vec![], "Webhook message delivery failed with error: Microsoft Teams endpoint returned HTTP error 400"),
        ]);
        send_teams(&base, &failing()).unwrap();
        let err = send_teams(&base, &failing()).unwrap_err();
        assert!(
            matches!(&err, NotifyError::Rejected { service: "Microsoft Teams", status: 200, body } if body.contains("delivery failed"))
        );
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "a permanent in-body error is not retried"
        );
    }

    #[test]
    fn slack_honours_retry_after() {
        let (base, log) = serve(vec![
            (429, vec![("Retry-After", "1")], "rate_limited"),
            (200, vec![], "ok"),
        ]);
        let start = Instant::now();
        send_slack(&base, &failing()).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(950));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].body, log[1].body);
        assert!(log[1].json()["blocks"].is_array());
    }

    #[test]
    fn server_errors_retry_three_times_with_backoff() {
        let (base, log) = serve(vec![
            (500, vec![], "boom"),
            (503, vec![], ""),
            (500, vec![], "still boom"),
        ]);
        let start = Instant::now();
        let err = send_webhook(&base, &[], None, &failing()).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(2900),
            "1 s + 2 s of backoff, took {elapsed:?}"
        );
        assert_eq!(log.lock().unwrap().len(), 3);
        assert_eq!(err.to_string(), "The webhook answered 500: still boom");
    }

    #[test]
    fn client_errors_fail_at_once_with_a_clipped_body() {
        let long: &'static str = Box::leak("bad payload ".repeat(60).into_boxed_str());
        let (base, log) = serve(vec![(400, vec![], long)]);
        let err = send_webhook(&base, &[], None, &failing()).unwrap_err();
        match err {
            NotifyError::Rejected {
                status: 400, body, ..
            } => assert!(body.chars().count() <= MAX_ERROR_BODY && body.ends_with('\u{2026}')),
            other => panic!("{other:?}"),
        }
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn redirects_are_not_followed() {
        let (base, log) = serve(vec![(
            302,
            vec![("Location", "http://127.0.0.1:9/elsewhere")],
            "",
        )]);
        let err = send_webhook(&base, &[], None, &failing()).unwrap_err();
        assert!(matches!(err, NotifyError::Rejected { status: 302, .. }));
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn webhook_signs_and_sends_headers() {
        let (base, log) = serve(vec![(204, vec![], "")]);
        let n = failing();
        send_webhook(
            &format!("{base}/hook"),
            &[("X-Api-Key".into(), "k1".into())],
            Some("s3cret"),
            &n,
        )
        .unwrap();
        let log = log.lock().unwrap();
        let req = &log[0];
        assert_eq!(req.header("x-api-key"), Some("k1"));
        assert_eq!(
            req.header("user-agent"),
            Some(format!("PCompare/{}", n.app_version).as_str())
        );
        assert_eq!(
            req.header(SIGNATURE_HEADER),
            Some(signature("s3cret", &req.body).as_str())
        );
        let ts: u64 = req.header(TIMESTAMP_HEADER).unwrap().parse().unwrap();
        assert!(ts > 1_700_000_000);
        assert_eq!(req.json()["event"], "pcompare.sync.failure");
    }

    #[test]
    fn unsigned_webhook_has_no_signature_headers() {
        let (base, log) = serve(vec![(200, vec![], "{}")]);
        send_webhook(&base, &[], None, &failing()).unwrap();
        assert!(log.lock().unwrap()[0].header(SIGNATURE_HEADER).is_none());
    }

    #[test]
    fn unreachable_errors_hide_the_url() {
        // Bind then drop to get a port nobody listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = send_slack(
            &format!("http://127.0.0.1:{port}/services/SECRET-TOKEN"),
            &failing(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, NotifyError::Transport(_)));
        assert!(msg.starts_with("Couldn't reach 127.0.0.1"), "{msg}");
        assert!(!msg.contains("SECRET"), "{msg}");
    }

    #[test]
    fn send_works_from_spawn_blocking() {
        let (base, _log) = serve(vec![(202, vec![], "")]);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let n = failing();
        let r = rt.block_on(async move {
            tokio::task::spawn_blocking(move || send_teams(&base, &n))
                .await
                .unwrap()
        });
        r.unwrap();
    }
}
