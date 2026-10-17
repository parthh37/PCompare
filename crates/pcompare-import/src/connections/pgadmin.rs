//! pgAdmin 4 `servers.json` (Tools → Import/Export Servers, or
//! `setup.py dump-servers`).
//!
//! ```json
//! { "Servers": { "1": {
//!     "Name": "Prod", "Group": "Servers", "Host": "pg.example.com", "Port": 5432,
//!     "MaintenanceDB": "postgres", "Username": "admin",
//!     "ConnectionParameters": { "sslmode": "require" } } } }
//! ```
//!
//! Older exports keep `SSLMode` at the top level.

use super::{ConnectionDraft, Engine, ImportedConnection, Parsed, Tls};
use crate::{Result, Skipped};
use serde_json::Value;

/// pgAdmin's default group; servers in it get no group.
const DEFAULT_GROUP: &str = "Servers";

pub(crate) fn is_servers_json(v: &Value) -> bool {
    v.get("Servers").is_some_and(Value::is_object)
}

pub(crate) fn parse(v: &Value) -> Result<Parsed> {
    let mut out = Parsed {
        label: "pgAdmin".into(),
        ..Default::default()
    };
    let Some(servers) = v.get("Servers").and_then(Value::as_object) else {
        return Ok(out);
    };
    // Exports number servers "1", "2"…; keep that order.
    let mut entries: Vec<(&String, &Value)> = servers.iter().collect();
    entries.sort_by_key(|(k, _)| k.parse::<u64>().unwrap_or(u64::MAX));
    for (_, s) in entries {
        let text = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default().trim();
        let name = text("Name");
        let params = s.get("ConnectionParameters");
        let param = |k: &str| params.and_then(|p| p.get(k)).and_then(Value::as_str).unwrap_or_default();
        let mut cfg = ConnectionDraft::new(Engine::Postgres);
        let mut notes = Vec::new();
        let host = match text("Host") {
            "" => text("HostAddr"),
            h => h,
        };
        let host = if host.is_empty() { param("host") } else { host };
        if host.is_empty() && !text("Service").is_empty() {
            out.skipped.push(Skipped {
                name: name.to_owned(),
                reason: "uses a pg_service.conf service, which PCompare can't read".into(),
            });
            continue;
        }
        cfg.host = if host.is_empty() || host.starts_with('/') {
            "localhost".into()
        } else {
            host.to_owned()
        };
        cfg.port = s
            .get("Port")
            .and_then(|p| p.as_u64().or_else(|| p.as_str().and_then(|t| t.parse().ok())))
            .and_then(|p| u16::try_from(p).ok());
        cfg.database = text("MaintenanceDB").to_owned();
        cfg.user = match text("Username") {
            // Shared servers name each user's login separately.
            "" => text("SharedUsername"),
            u => u,
        }
        .to_owned();
        let ssl = match param("sslmode") {
            "" => text("SSLMode"),
            m => m,
        };
        if !ssl.is_empty() {
            cfg.tls = match ssl {
                "disable" => Tls::Disable,
                "allow" | "prefer" => Tls::Prefer,
                _ => Tls::Require,
            };
        }
        if s.get("UseSSHTunnel").and_then(Value::as_u64).unwrap_or(0) != 0
            || s.get("UseSSHTunnel").and_then(Value::as_bool) == Some(true)
        {
            notes.push("Was reached through an SSH tunnel, which PCompare doesn't support yet".into());
        }
        let group = Some(text("Group")).filter(|g| !g.is_empty() && *g != DEFAULT_GROUP).map(str::to_owned);
        let label = cfg.label();
        out.connections.push(ImportedConnection {
            name: if name.is_empty() { label } else { name.to_owned() },
            group,
            config: cfg,
            notes,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_new_and_old_ssl_layouts_and_groups() {
        let v: Value = serde_json::from_str(
            r#"{ "Servers": {
              "2": { "Name": "Old", "Group": "Servers", "Host": "old.local", "Port": 5433,
                     "MaintenanceDB": "postgres", "Username": "me", "SSLMode": "disable" },
              "1": { "Name": "Prod", "Group": "Production", "Host": "pg.example.com", "Port": 5432,
                     "MaintenanceDB": "app", "Username": "admin",
                     "ConnectionParameters": { "sslmode": "verify-full", "connect_timeout": 10 } },
              "3": { "Name": "Svc", "Group": "Servers", "Service": "mydb", "Port": 5432 }
            } }"#,
        )
        .unwrap();
        assert!(is_servers_json(&v));
        let p = parse(&v).unwrap();
        assert_eq!(p.connections.len(), 2);
        let prod = &p.connections[0];
        assert_eq!((prod.name.as_str(), prod.group.as_deref()), ("Prod", Some("Production")));
        assert_eq!((prod.config.database.as_str(), prod.config.tls), ("app", Tls::Require));
        let old = &p.connections[1];
        assert_eq!((old.group.as_deref(), old.config.port, old.config.tls), (None, Some(5433), Tls::Disable));
        assert_eq!(p.skipped.len(), 1);
    }
}
