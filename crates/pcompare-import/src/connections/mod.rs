//! Reads saved database connections from database tools and maps each one
//! onto a PCompare connection (engine, server, database, login, encryption).
//!
//! Only connection details come over. Passwords stored by those tools are
//! never read; the app asks for them (SQL logins) or needs none (Windows
//! authentication). Files are recognised by content, not extension, and
//! nothing here writes.

pub mod conn_string;
mod dbeaver;
mod heidisql;
mod jetbrains;
mod pgadmin;
mod ssms;
mod vscode;
mod workbench;

use crate::{format_err, ImportError, Result, Skipped};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    #[default]
    Mssql,
    Mysql,
    Postgres,
    Sqlite,
}

impl Engine {
    pub fn default_port(self) -> Option<u16> {
        match self {
            Engine::Mssql => Some(1433),
            Engine::Mysql => Some(3306),
            Engine::Postgres => Some(5432),
            Engine::Sqlite => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Auth {
    #[default]
    Password,
    /// SQL Server Windows authentication.
    Integrated,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Tls {
    Disable,
    #[default]
    Prefer,
    Require,
}

/// The same JSON shape as `pcompare_db::connection::ConnectionConfig`, so
/// the app saves it as-is (and this crate stays free of database drivers).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionDraft {
    pub engine: Engine,
    pub host: String,
    /// `None` = the engine's default port.
    pub port: Option<u16>,
    /// SQL Server named instance.
    pub instance: String,
    pub database: String,
    pub user: String,
    pub auth: Auth,
    pub tls: Tls,
    pub trust_server_certificate: bool,
    /// SQLite database file.
    pub file: String,
    pub schemas: Vec<String>,
}

impl ConnectionDraft {
    pub(crate) fn new(engine: Engine) -> Self {
        Self {
            engine,
            // The app's own default for SQL Server, where self-signed
            // certificates are the norm.
            trust_server_certificate: engine == Engine::Mssql,
            ..Default::default()
        }
    }

    /// `host\instance` or `host:port`, plus `/database`, for default names.
    pub(crate) fn label(&self) -> String {
        if self.engine == Engine::Sqlite {
            return leaf(&self.file).to_owned();
        }
        let mut s = self.host.clone();
        if !self.instance.is_empty() {
            s = format!("{s}\\{}", self.instance);
        } else if let Some(p) = self.port.filter(|&p| Some(p) != self.engine.default_port()) {
            s = format!("{s}:{p}");
        }
        if !self.database.is_empty() {
            s = format!("{s}/{}", self.database);
        }
        s
    }
}

/// One connection read from another tool, already in PCompare's terms.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedConnection {
    pub name: String,
    /// The folder or server group it sits in ("Prod/EU"), if the tool has them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub config: ConnectionDraft,
    /// Settings that had no PCompare equivalent, or were adjusted.
    pub notes: Vec<String>,
}

/// Everything read from one file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSource {
    /// The tool (and version, when known), e.g. "SQL Server Management Studio 20".
    pub label: String,
    pub path: String,
    pub connections: Vec<ImportedConnection>,
    pub skipped: Vec<Skipped>,
}

/// What a single parser returns; [`read`] adds the path.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub label: String,
    pub connections: Vec<ImportedConnection>,
    pub skipped: Vec<Skipped>,
}

/// A connection list found where a tool keeps it.
pub use crate::Candidate;

/// Reads any supported connection file, recognised by its content.
pub fn read(path: &Path) -> Result<ConnectionSource> {
    let bytes = std::fs::read(path).map_err(|source| ImportError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let parsed = parse_bytes(&bytes, path)?;
    Ok(ConnectionSource {
        label: parsed.label,
        path: path.display().to_string(),
        connections: parsed.connections,
        skipped: parsed.skipped,
    })
}

pub(crate) fn parse_bytes(bytes: &[u8], path: &Path) -> Result<Parsed> {
    let text = crate::decode(bytes);
    let trimmed = text.trim_start();
    if trimmed.starts_with('<') {
        let doc = roxmltree::Document::parse_with_options(
            &text,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .map_err(|e| format_err(format!("not valid XML: {e}")))?;
        let root = doc.root_element();
        return match root.tag_name().name() {
            "SqlStudio" => ssms::parse_user_settings(root, path),
            "data" if root.attribute("grt_format").is_some() => workbench::parse(root),
            _ if ssms::is_registered_servers(root) => ssms::parse_registered_servers(root, path),
            _ if jetbrains::is_data_sources(root) => jetbrains::parse(root, path),
            other => Err(format_err(format!(
                "`<{other}>` files aren't a connection list PCompare knows"
            ))),
        };
    }
    if trimmed.starts_with('{') {
        let v = parse_jsonc(&text)?;
        if pgadmin::is_servers_json(&v) {
            return pgadmin::parse(&v);
        }
        if dbeaver::is_data_sources(&v) {
            return Ok(dbeaver::parse(&v, path));
        }
        if vscode::is_settings(&v) {
            return Ok(vscode::parse(&v, path));
        }
        return Err(format_err("this JSON file has no database connections PCompare recognises"));
    }
    if trimmed.to_ascii_lowercase().starts_with("[oledb]") {
        return udl(trimmed, path);
    }
    if heidisql::is_settings(&text) {
        return Ok(heidisql::parse(&text));
    }
    Err(format_err("not a connection list PCompare recognises"))
}

/// A Windows data link (`.udl`): `[oledb]`, a comment, then one OLE DB
/// connection string. Its password, if saved, is ignored.
fn udl(text: &str, path: &Path) -> Result<Parsed> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('[') && !l.starts_with(';'))
        .ok_or_else(|| format_err("this data link file has no connection string"))?;
    let parsed = conn_string::parse(line, None).map_err(format_err)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map_or_else(|| parsed.config.label(), str::to_owned);
    Ok(Parsed {
        label: "Data link file".into(),
        connections: vec![ImportedConnection {
            name,
            group: None,
            config: parsed.config,
            notes: parsed.notes,
        }],
        skipped: Vec::new(),
    })
}

/// Connection lists of database tools installed for this user, in their
/// default places.
pub fn discover() -> Vec<Candidate> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let (appdata, home) = (env("APPDATA"), env("HOME").or_else(|| env("USERPROFILE")));
    let (xdg_config, xdg_data) = (env("XDG_CONFIG_HOME"), env("XDG_DATA_HOME"));
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    found.extend(ssms::locations(appdata.clone()));
    found.extend(vscode::locations(appdata.clone(), home.clone(), xdg_config.clone()));
    found.extend(dbeaver::locations(appdata.clone(), home.clone(), xdg_data));
    found.extend(jetbrains::locations(appdata.clone(), home.clone(), xdg_config));
    found.extend(workbench::locations(appdata, home));
    let mut out: Vec<Candidate> = Vec::new();
    for (label, path) in found {
        if !path.is_file() || out.iter().any(|c| Path::new(&c.path) == path) {
            continue;
        }
        // Editor settings only count when they hold connections.
        if path.file_name().is_some_and(|n| n == "settings.json")
            && !std::fs::read(&path).is_ok_and(|b| vscode::mentions_connections(&crate::decode(&b)))
        {
            continue;
        }
        out.push(Candidate {
            label,
            path: path.display().to_string(),
        });
    }
    out
}

/// Parses a single pasted connection string into a connection. The password
/// in it (if any) is returned separately, for the dialog's password field.
pub fn from_connection_string(text: &str) -> Result<(ImportedConnection, Option<String>)> {
    let parsed = conn_string::parse(text, None).map_err(format_err)?;
    let conn = ImportedConnection {
        name: parsed.config.label(),
        group: None,
        config: parsed.config,
        notes: parsed.notes,
    };
    Ok((conn, parsed.password))
}

// ---------- helpers shared by the parsers ----------

/// The last path component.
pub(crate) fn leaf(p: &str) -> &str {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(t)
}

/// `true`/`yes`/`1`/`sspi` (any case) — the spellings connection strings and
/// tool settings use for "on".
pub(crate) fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "1" | "sspi" | "on"
    )
}

/// SQL Server's `Encrypt` setting (Microsoft.Data.SqlClient 4+ spellings,
/// plus the older booleans).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encrypt {
    Optional,
    Mandatory,
    Strict,
}

impl Encrypt {
    pub(crate) fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "strict" => Some(Encrypt::Strict),
            "true" | "yes" | "1" | "mandatory" | "required" | "require" => Some(Encrypt::Mandatory),
            "false" | "no" | "0" | "optional" | "" => Some(Encrypt::Optional),
            _ => None,
        }
    }
}

/// Maps SQL Server encryption settings onto PCompare's. `Optional` clients
/// never validated the certificate, while PCompare's "Prefer" encrypts and
/// validates, so it carries over as Prefer + trust to keep connecting the
/// way it did.
pub(crate) fn sql_server_tls(encrypt: Encrypt, trust: bool, cfg: &mut ConnectionDraft) {
    (cfg.tls, cfg.trust_server_certificate) = match encrypt {
        Encrypt::Optional => (Tls::Prefer, true),
        Encrypt::Mandatory => (Tls::Require, trust),
        // TDS 8 always validates the certificate.
        Encrypt::Strict => (Tls::Require, false),
    };
}

/// A SQL Server "server name" as the tools write it:
/// `[tcp:|np:|lpc:|admin:]host[\instance][,port]`, `.` / `(local)` for this
/// computer and `(localdb)\name` for LocalDB.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SqlServerAddress {
    pub host: String,
    pub instance: String,
    pub port: Option<u16>,
    pub notes: Vec<String>,
}

/// `Err` = a server PCompare can't reach (LocalDB), with the reason.
pub(crate) fn sql_server_address(server: &str) -> std::result::Result<SqlServerAddress, String> {
    let mut out = SqlServerAddress::default();
    let mut s = server.trim();
    if let Some((proto, rest)) = s.split_once(':') {
        match proto.to_ascii_lowercase().as_str() {
            "tcp" => s = rest,
            "lpc" => s = rest,
            "admin" => {
                out.notes.push("Uses a regular connection, not the dedicated admin connection".into());
                s = rest;
            }
            "np" => {
                out.notes.push("Connects over TCP; PCompare can't use named pipes".into());
                // `np:\\host\pipe\MSSQL$INST\sql\query` or `np:host`.
                let pipe = rest.trim_start_matches('\\');
                let host = pipe.split('\\').next().unwrap_or_default();
                let instance = pipe
                    .split('\\')
                    .find_map(|seg| seg.strip_prefix("MSSQL$"))
                    .unwrap_or_default();
                out.host = local_host(host);
                out.instance = instance.to_owned();
                return Ok(out);
            }
            _ => {}
        }
    }
    let s = s.trim();
    if s.to_ascii_lowercase().starts_with("(localdb)") {
        return Err("LocalDB only accepts named-pipe connections, which PCompare can't use".into());
    }
    let (rest, port) = match s.rsplit_once(',') {
        Some((h, p)) => match p.trim().parse::<u16>() {
            Ok(p) => (h.trim(), Some(p)),
            Err(_) => (s, None),
        },
        None => match s.rsplit_once(':') {
            // `host:port` isn't SQL Server syntax, but people write it.
            Some((h, p)) if !h.contains(':') && !h.is_empty() => match p.parse::<u16>() {
                Ok(p) => (h, Some(p)),
                Err(_) => (s, None),
            },
            _ => (s, None),
        },
    };
    let (host, instance) = match rest.split_once('\\') {
        Some((h, i)) => (h, i.trim()),
        None => (rest, ""),
    };
    out.host = local_host(host.trim().trim_start_matches('[').trim_end_matches(']'));
    out.port = port;
    // With a port, SQL Server clients ignore the instance name.
    if port.is_none() && !instance.eq_ignore_ascii_case("MSSQLSERVER") {
        out.instance = instance.to_owned();
    }
    Ok(out)
}

/// `.`, `(local)` and an empty name all mean this computer.
fn local_host(h: &str) -> String {
    match h.trim() {
        "" | "." => "localhost".into(),
        h if h.eq_ignore_ascii_case("(local)") => "localhost".into(),
        h => h.to_owned(),
    }
}

/// Applies a parsed SQL Server address to a draft, carrying its notes over.
pub(crate) fn apply_sql_server(
    server: &str,
    cfg: &mut ConnectionDraft,
    notes: &mut Vec<String>,
) -> std::result::Result<(), String> {
    let a = sql_server_address(server)?;
    cfg.host = a.host;
    cfg.instance = a.instance;
    cfg.port = a.port;
    notes.extend(a.notes);
    Ok(())
}

/// Removes `//` and `/* */` comments and trailing commas, so the JSON-with-
/// comments that editors write for `settings.json` parses as JSON.
pub(crate) fn strip_jsonc(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match (c, b.get(i + 1)) {
            (b'"', _) => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            (b'/', Some(b'/')) => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            (b',', _) => {
                // Drop the comma when only whitespace/comments lead to `}` or `]`.
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if !matches!(b.get(j), Some(b'}' | b']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Parses `settings.json`-style JSON with comments and trailing commas.
pub(crate) fn parse_jsonc(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(&strip_jsonc(text)).map_err(|e| format_err(format!("not valid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_server_names_split_into_host_instance_and_port() {
        let a = sql_server_address(r"tcp:db01\SQLEXPRESS").unwrap();
        assert_eq!((a.host.as_str(), a.instance.as_str(), a.port), ("db01", "SQLEXPRESS", None));
        let a = sql_server_address("db01.corp.local,14330").unwrap();
        assert_eq!((a.host.as_str(), a.port), ("db01.corp.local", Some(14330)));
        // A port wins over the instance, as in SQL Server's own clients.
        let a = sql_server_address(r"db01\INST,1500").unwrap();
        assert_eq!((a.instance.as_str(), a.port), ("", Some(1500)));
        assert_eq!(sql_server_address(".").unwrap().host, "localhost");
        let a = sql_server_address(r"(local)\SQL2022").unwrap();
        assert_eq!((a.host.as_str(), a.instance.as_str()), ("localhost", "SQL2022"));
        assert_eq!(sql_server_address(r"host\MSSQLSERVER").unwrap().instance, "");
        assert_eq!(sql_server_address("host:1444").unwrap().port, Some(1444));
        assert!(sql_server_address(r"(localdb)\MSSQLLocalDB").is_err());
        let a = sql_server_address(r"np:\\fs01\pipe\MSSQL$PROD\sql\query").unwrap();
        assert_eq!((a.host.as_str(), a.instance.as_str()), ("fs01", "PROD"));
        assert_eq!(a.notes.len(), 1);
    }

    #[test]
    fn optional_encryption_keeps_trusting_the_certificate() {
        let mut c = ConnectionDraft::new(Engine::Mssql);
        sql_server_tls(Encrypt::Optional, false, &mut c);
        assert_eq!((c.tls, c.trust_server_certificate), (Tls::Prefer, true));
        sql_server_tls(Encrypt::Mandatory, false, &mut c);
        assert_eq!((c.tls, c.trust_server_certificate), (Tls::Require, false));
        sql_server_tls(Encrypt::Strict, true, &mut c);
        assert_eq!((c.tls, c.trust_server_certificate), (Tls::Require, false));
        assert_eq!(Encrypt::parse("Mandatory"), Some(Encrypt::Mandatory));
        assert_eq!(Encrypt::parse("False"), Some(Encrypt::Optional));
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_removed() {
        let v = parse_jsonc(
            r#"{
              // editor settings
              "a": "http://x // not a comment", /* block */
              "b": [1, 2,],
            }"#,
        )
        .unwrap();
        assert_eq!(v["a"], "http://x // not a comment");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn data_link_files_and_unknown_files() {
        let mut udl = vec![0xFF, 0xFE];
        let text = "[oledb]\r\n; Everything after this line is an OLE DB initstring\r\nProvider=MSOLEDBSQL.1;Integrated Security=SSPI;Persist Security Info=False;Initial Catalog=Ops;Data Source=sql9\r\n";
        udl.extend(text.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let p = parse_bytes(&udl, Path::new("ops.udl")).unwrap();
        let c = &p.connections[0];
        assert_eq!((c.name.as_str(), c.config.host.as_str(), c.config.auth), ("ops", "sql9", Auth::Integrated));
        assert!(parse_bytes(b"{\"editor.fontSize\": 12}", Path::new("settings.json")).is_err());
        assert!(parse_bytes(b"<svg/>", Path::new("x.xml")).is_err());
    }

    #[test]
    fn labels_skip_default_ports() {
        let mut c = ConnectionDraft::new(Engine::Postgres);
        c.host = "pg".into();
        c.port = Some(5432);
        c.database = "app".into();
        assert_eq!(c.label(), "pg/app");
        c.port = Some(6543);
        assert_eq!(c.label(), "pg:6543/app");
    }
}
