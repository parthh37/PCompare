//! Editor `settings.json` files (JSON with comments), which hold the
//! connections of three tools:
//!
//! - Azure Data Studio: `datasource.connections` + `datasource.connectionGroups`
//!   (`providerName` MSSQL / PGSQL / MySQL, fields under `options`)
//! - the VS Code SQL Server extension: `mssql.connections` + `mssql.connectionGroups`
//! - SQLTools: `sqltools.connections` (a flat `group` string)
//!
//! Group trees use `id` / `parentId`, with a root named `ROOT`. A
//! `.code-workspace` file keeps the same keys under `"settings"`.
//! Passwords (`password`, connection strings that embed one) are ignored.

use super::conn_string;
use super::{
    apply_sql_server, sql_server_tls, truthy, Auth, ConnectionDraft, Encrypt, Engine, ImportedConnection, Parsed, Tls,
};
use crate::Skipped;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ADS: &str = "datasource.connections";
const MSSQL: &str = "mssql.connections";
const SQLTOOLS: &str = "sqltools.connections";
/// Azure Data Studio's fixed root-group id.
const ADS_ROOT: &str = "C777F06B-202E-4480-B475-FA416154D458";

/// The object holding the settings: the file itself, or a workspace file's `"settings"`.
fn settings(v: &Value) -> Option<&Value> {
    let has = |o: &Value| [ADS, MSSQL, SQLTOOLS].iter().any(|k| o.get(k).is_some_and(Value::is_array));
    if has(v) {
        return Some(v);
    }
    v.get("settings").filter(|s| has(s))
}

pub(crate) fn is_settings(v: &Value) -> bool {
    settings(v).is_some()
}

/// A cheap check for discovery: does this settings file mention connections?
pub(crate) fn mentions_connections(text: &str) -> bool {
    [ADS, MSSQL, SQLTOOLS].iter().any(|k| text.contains(&format!("\"{k}\"")))
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).map(str::trim).unwrap_or_default()
}

fn port(v: &Value, k: &str) -> Option<u16> {
    let p = v.get(k)?;
    p.as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| p.as_str().and_then(|t| t.trim().parse().ok()))
        .filter(|&n| n != 0)
}

/// `true` / `"true"` / `"Mandatory"`… as SQL Server's encrypt setting.
fn encrypt(v: Option<&Value>, default: Encrypt) -> Encrypt {
    match v {
        Some(Value::Bool(b)) => if *b { Encrypt::Mandatory } else { Encrypt::Optional },
        Some(Value::String(t)) => Encrypt::parse(t).unwrap_or(default),
        _ => default,
    }
}

fn flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(t)) => truthy(t),
        _ => false,
    }
}

/// `id → (name, parentId)` from a group list; a root group has no name here.
fn group_tree(list: Option<&Value>) -> HashMap<String, (String, String)> {
    // Settings sync has been seen turning the list into a string; ignore that.
    let Some(groups) = list.and_then(Value::as_array) else {
        return HashMap::new();
    };
    groups
        .iter()
        .filter_map(|g| {
            let id = s(g, "id");
            let name = s(g, "name");
            let root = id.eq_ignore_ascii_case(ADS_ROOT)
                || id.eq_ignore_ascii_case("ROOT")
                || name.is_empty()
                || name == "/"
                || name.eq_ignore_ascii_case("ROOT");
            (!id.is_empty()).then(|| (id.to_owned(), (if root { String::new() } else { name.to_owned() }, s(g, "parentId").to_owned())))
        })
        .collect()
}

/// `Prod/EU` for a group id, walking up to the root.
fn group_path(tree: &HashMap<String, (String, String)>, id: &str) -> Option<String> {
    let mut names = Vec::new();
    let mut cur = id;
    for _ in 0..64 {
        let Some((name, parent)) = tree.get(cur) else { break };
        if name.is_empty() {
            break;
        }
        names.push(name.as_str());
        cur = parent;
    }
    names.reverse();
    Some(names.join("/")).filter(|p| !p.is_empty())
}

fn push(out: &mut Parsed, name: &str, r: std::result::Result<(ConnectionDraft, Vec<String>), String>, group: Option<String>) {
    match r {
        Ok((config, notes)) => out.connections.push(ImportedConnection {
            name: if name.is_empty() { config.label() } else { name.to_owned() },
            group,
            config,
            notes,
        }),
        Err(reason) => out.skipped.push(Skipped {
            name: if name.is_empty() { "(unnamed)".into() } else { name.to_owned() },
            reason,
        }),
    }
}

/// "Azure Data Studio", "VS Code", "VSCodium"… from where the file lives.
fn label(path: &Path, v: &Value) -> String {
    let p = path.to_string_lossy().to_lowercase();
    let editor = if p.contains("azuredatastudio") {
        "Azure Data Studio"
    } else if p.contains("code - insiders") {
        "VS Code Insiders"
    } else if p.contains("vscodium") {
        "VSCodium"
    } else if p.contains("cursor") {
        "Cursor"
    } else if p.contains("code") {
        "VS Code"
    } else if v.get(ADS).is_some() {
        "Azure Data Studio"
    } else {
        "Editor settings"
    };
    if path.extension().is_some_and(|x| x == "code-workspace") {
        format!("{editor} workspace")
    } else {
        editor.to_owned()
    }
}

pub(crate) fn parse(v: &Value, path: &Path) -> Parsed {
    let root = settings(v).unwrap_or(v);
    let mut out = Parsed {
        label: label(path, root),
        ..Default::default()
    };
    let base = path.parent().map(Path::to_path_buf);

    let ads_groups = group_tree(root.get("datasource.connectionGroups"));
    for c in root.get(ADS).and_then(Value::as_array).into_iter().flatten() {
        let o = c.get("options").unwrap_or(c);
        let name = s(o, "connectionName");
        let gid = match s(c, "groupId") {
            "" => s(o, "groupId"),
            g => g,
        };
        push(&mut out, name, ads_connection(c, o), group_path(&ads_groups, gid));
    }

    let mssql_groups = group_tree(root.get("mssql.connectionGroups"));
    for c in root.get(MSSQL).and_then(Value::as_array).into_iter().flatten() {
        push(&mut out, s(c, "profileName"), mssql_connection(c), group_path(&mssql_groups, s(c, "groupId")));
    }

    for c in root.get(SQLTOOLS).and_then(Value::as_array).into_iter().flatten() {
        let group = Some(s(c, "group").to_owned()).filter(|g| !g.is_empty());
        push(&mut out, s(c, "name"), sqltools_connection(c, base.as_deref()), group);
    }
    out
}

/// SQL Server's sign-in types in these tools (`SqlLogin`, `Integrated`, Entra variants).
fn auth(kind: &str, cfg: &mut ConnectionDraft, notes: &mut Vec<String>) {
    match kind.to_ascii_lowercase().as_str() {
        "integrated" => cfg.auth = Auth::Integrated,
        "sqllogin" | "" => cfg.auth = Auth::Password,
        other => notes.push(format!(
            "Signs in with Microsoft Entra ID ({other}), which PCompare doesn't support yet; enter a SQL login instead"
        )),
    }
}

fn ads_connection(c: &Value, o: &Value) -> std::result::Result<(ConnectionDraft, Vec<String>), String> {
    let provider = s(c, "providerName").to_ascii_uppercase();
    let mut notes = Vec::new();
    match provider.as_str() {
        "MSSQL" | "MSSQL-CMS" | "" => {
            let mut cfg = ConnectionDraft::new(Engine::Mssql);
            apply_sql_server(s(o, "server"), &mut cfg, &mut notes)?;
            cfg.port = cfg.port.or(port(o, "port"));
            cfg.database = s(o, "database").to_owned();
            auth(s(o, "authenticationType"), &mut cfg, &mut notes);
            if cfg.auth == Auth::Password {
                cfg.user = s(o, "user").to_owned();
            }
            // Azure Data Studio has encrypted by default since early 2023.
            sql_server_tls(encrypt(o.get("encrypt"), Encrypt::Mandatory), flag(o.get("trustServerCertificate")), &mut cfg);
            Ok((cfg, notes))
        }
        "PGSQL" | "MYSQL" => {
            let engine = if provider == "PGSQL" { Engine::Postgres } else { Engine::Mysql };
            let mut cfg = ConnectionDraft::new(engine);
            cfg.host = match s(o, "host") {
                "" => "localhost".into(),
                h => h.to_owned(),
            };
            cfg.port = port(o, "port");
            cfg.database = s(o, "dbname").to_owned();
            cfg.user = s(o, "user").to_owned();
            let ssl = if engine == Engine::Postgres { s(o, "sslmode") } else { s(o, "ssl") };
            // The MySQL extension's default is `require`.
            let ssl = if ssl.is_empty() && engine == Engine::Mysql { "require" } else { ssl };
            set_ssl(&mut cfg, ssl);
            if !s(o, "authenticationType").eq_ignore_ascii_case("SqlLogin") && !s(o, "authenticationType").is_empty() {
                notes.push("Signs in with Microsoft Entra ID, which PCompare doesn't support yet; enter a login instead".into());
            }
            Ok((cfg, notes))
        }
        other => Err(format!("{other} connections aren't a database PCompare supports")),
    }
}

/// PostgreSQL `sslmode` / MySQL `ssl` words (with `_` or `-`).
fn set_ssl(cfg: &mut ConnectionDraft, v: &str) {
    cfg.tls = match v.to_ascii_lowercase().replace('_', "-").as_str() {
        "" => cfg.tls,
        "disable" | "disabled" => Tls::Disable,
        "allow" | "prefer" | "preferred" => Tls::Prefer,
        _ => Tls::Require,
    };
}

fn mssql_connection(c: &Value) -> std::result::Result<(ConnectionDraft, Vec<String>), String> {
    let mut notes = Vec::new();
    if s(c, "server").is_empty() && !s(c, "connectionString").is_empty() {
        // The password in the string (if any) is dropped.
        let p = conn_string::parse(s(c, "connectionString"), Some(Engine::Mssql))?;
        return Ok((p.config, p.notes));
    }
    let mut cfg = ConnectionDraft::new(Engine::Mssql);
    apply_sql_server(s(c, "server"), &mut cfg, &mut notes)?;
    cfg.port = cfg.port.or(port(c, "port"));
    cfg.database = s(c, "database").to_owned();
    auth(s(c, "authenticationType"), &mut cfg, &mut notes);
    if cfg.auth == Auth::Password {
        cfg.user = s(c, "user").to_owned();
    }
    sql_server_tls(encrypt(c.get("encrypt"), Encrypt::Mandatory), flag(c.get("trustServerCertificate")), &mut cfg);
    Ok((cfg, notes))
}

fn sqltools_connection(c: &Value, base: Option<&Path>) -> std::result::Result<(ConnectionDraft, Vec<String>), String> {
    let driver = s(c, "driver");
    let engine = match driver.to_ascii_lowercase().as_str() {
        "mssql" => Engine::Mssql,
        "mysql" | "mariadb" => Engine::Mysql,
        "postgresql" => Engine::Postgres,
        "sqlite" => Engine::Sqlite,
        _ => return Err(format!("{driver} isn't a database PCompare supports")),
    };
    let mut notes = Vec::new();
    if s(c, "connectionMethod").eq_ignore_ascii_case("Connection String") {
        let p = conn_string::parse(s(c, "connectString"), Some(engine))?;
        return Ok((p.config, p.notes));
    }
    let mut cfg = ConnectionDraft::new(engine);
    if engine == Engine::Sqlite {
        let file = s(c, "database");
        if file.is_empty() || file == ":memory:" {
            return Err("in-memory SQLite databases can't be compared".into());
        }
        cfg.file = match base {
            // Relative to the workspace; a user settings file has none.
            Some(dir) if Path::new(file).is_relative() && dir.ends_with(".vscode") => {
                dir.parent().unwrap_or(dir).join(file).display().to_string()
            }
            _ => file.to_owned(),
        };
        if Path::new(&cfg.file).is_relative() && !cfg.file.starts_with('/') && !cfg.file.contains(':') {
            notes.push("The file path is relative to a VS Code workspace; check it after importing".into());
        }
        return Ok((cfg, notes));
    }
    if s(c, "connectionMethod").eq_ignore_ascii_case("Socket File") {
        notes.push("Connected through a socket file; PCompare uses TCP on localhost".into());
        cfg.host = "localhost".into();
    } else if engine == Engine::Mssql {
        let mut server = s(c, "server").to_owned();
        let opts = c.get("mssqlOptions");
        let instance = opts.map(|o| s(o, "instanceName")).unwrap_or_default();
        if !instance.is_empty() && !server.contains('\\') {
            server = format!("{server}\\{instance}");
        }
        apply_sql_server(&server, &mut cfg, &mut notes)?;
        let enc = encrypt(opts.and_then(|o| o.get("encrypt")), Encrypt::Mandatory);
        sql_server_tls(enc, flag(opts.and_then(|o| o.get("trustServerCertificate"))), &mut cfg);
        if !s(c, "domain").is_empty() {
            notes.push(format!(
                "Signed in to the {} domain with a named account (NTLM); PCompare uses Windows authentication or a SQL login",
                s(c, "domain")
            ));
        }
    } else {
        cfg.host = match s(c, "server") {
            "" => "localhost".into(),
            h => h.to_owned(),
        };
        let opts = c.get(if engine == Engine::Postgres { "pgOptions" } else { "mysqlOptions" });
        cfg.tls = match (opts.and_then(|o| o.get("ssl")), opts.map(|o| s(o, "enableSsl"))) {
            (_, Some(e)) if e.eq_ignore_ascii_case("Disabled") => Tls::Disable,
            (Some(Value::Bool(false)), _) => Tls::Disable,
            (Some(Value::Bool(true) | Value::Object(_)), _) => Tls::Require,
            (_, Some(e)) if e.eq_ignore_ascii_case("Enabled") => Tls::Require,
            _ => cfg.tls,
        };
    }
    cfg.port = cfg.port.or(port(c, "port"));
    cfg.database = s(c, "database").to_owned();
    cfg.user = s(c, "username").to_owned();
    Ok((cfg, notes))
}

/// User settings of the editors that hold these connections, plus their
/// profiles: `<config>/<Editor>/User/settings.json`.
pub(crate) fn locations(appdata: Option<PathBuf>, home: Option<PathBuf>, xdg_config: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(a) = appdata {
        roots.push(a);
    }
    if let Some(h) = &home {
        roots.push(h.join("Library").join("Application Support"));
    }
    if let Some(x) = xdg_config.or_else(|| home.map(|h| h.join(".config"))) {
        roots.push(x);
    }
    let editors = [
        ("azuredatastudio", "Azure Data Studio"),
        ("Code", "VS Code"),
        ("Code - Insiders", "VS Code Insiders"),
        ("VSCodium", "VSCodium"),
        ("Cursor", "Cursor"),
    ];
    let mut out = Vec::new();
    for root in &roots {
        for (dir, label) in editors {
            let user = root.join(dir).join("User");
            out.push((label.to_owned(), user.join("settings.json")));
            let profiles = std::fs::read_dir(user.join("profiles")).into_iter().flatten().flatten();
            for p in profiles {
                out.push((format!("{label} (profile)"), p.path().join("settings.json")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_text(text: &str, path: &str) -> Parsed {
        let v = super::super::parse_jsonc(text).unwrap();
        assert!(is_settings(&v));
        parse(&v, Path::new(path))
    }

    #[test]
    fn azure_data_studio_groups_and_providers() {
        let p = parse_text(
            r#"{
              // user settings
              "workbench.colorTheme": "Default Dark+",
              "datasource.connectionGroups": [
                { "name": "ROOT", "id": "C777F06B-202E-4480-B475-FA416154D458" },
                { "name": "Prod", "id": "g1", "parentId": "C777F06B-202E-4480-B475-FA416154D458" },
                { "name": "EU", "id": "g2", "parentId": "g1" },
              ],
              "datasource.connections": [
                { "options": { "connectionName": "", "server": "sql01\\INST", "database": "Sales",
                    "authenticationType": "SqlLogin", "user": "app", "password": "",
                    "encrypt": "false", "trustServerCertificate": false, "groupId": "g2" },
                  "groupId": "g2", "providerName": "MSSQL", "savePassword": true, "id": "c1" },
                { "options": { "connectionName": "Local", "server": ".", "authenticationType": "Integrated",
                    "encrypt": true, "trustServerCertificate": true },
                  "groupId": "C777F06B-202E-4480-B475-FA416154D458", "providerName": "MSSQL", "id": "c2" },
                { "options": { "connectionName": "PG", "host": "pg", "port": 5433, "dbname": "app", "user": "me",
                    "sslmode": "verify_ca", "authenticationType": "SqlLogin" },
                  "groupId": "g1", "providerName": "PGSQL", "id": "c3" },
                { "options": { "connectionName": "Kusto" }, "providerName": "KUSTO", "id": "c4" }
              ]
            }"#,
            "/Users/me/Library/Application Support/azuredatastudio/User/settings.json",
        );
        assert_eq!(p.label, "Azure Data Studio");
        assert_eq!(p.connections.len(), 3);
        let a = &p.connections[0];
        assert_eq!((a.name.as_str(), a.group.as_deref()), (r"sql01\INST/Sales", Some("Prod/EU")));
        assert_eq!((a.config.instance.as_str(), a.config.user.as_str(), a.config.tls), ("INST", "app", Tls::Prefer));
        let b = &p.connections[1];
        assert_eq!((b.group.as_deref(), b.config.host.as_str(), b.config.auth, b.config.tls), (None, "localhost", Auth::Integrated, Tls::Require));
        let c = &p.connections[2];
        assert_eq!((c.config.engine, c.config.port, c.config.tls, c.group.as_deref()), (Engine::Postgres, Some(5433), Tls::Require, Some("Prod")));
        assert_eq!(p.skipped.len(), 1);
    }

    #[test]
    fn vscode_mssql_and_sqltools_in_one_file() {
        let p = parse_text(
            r#"{
              "mssql.connectionGroups": [ { "id": "ROOT", "name": "ROOT" }, { "id": "x", "name": "Dev", "parentId": "ROOT" } ],
              "mssql.connections": [
                { "server": "127.0.0.1,14002", "profileName": "Docker", "authenticationType": "SqlLogin",
                  "user": "sa", "password": "plaintext!", "encrypt": "Mandatory", "trustServerCertificate": true, "groupId": "x" },
                { "connectionString": "Server=tcp:legacy;Database=Ops;User Id=ops;Password=hunter2", "profileName": "Legacy" }
              ],
              "sqltools.connections": [
                { "name": "Shop", "driver": "MariaDB", "server": "db", "port": 3307, "database": "shop", "username": "root",
                  "password": "x", "mysqlOptions": { "ssl": true }, "group": "Web" },
                { "name": "Cache", "driver": "SQLite", "database": "./data/cache.db" },
                { "name": "Warehouse", "driver": "AWS Redshift", "server": "r" }
              ]
            }"#,
            "/home/me/.config/Code/User/settings.json",
        );
        assert_eq!(p.label, "VS Code");
        let names: Vec<_> = p.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Docker", "Legacy", "Shop", "Cache"]);
        let docker = &p.connections[0];
        assert_eq!((docker.group.as_deref(), docker.config.port, docker.config.tls, docker.config.trust_server_certificate), (Some("Dev"), Some(14002), Tls::Require, true));
        assert_eq!((p.connections[1].config.host.as_str(), p.connections[1].config.database.as_str()), ("legacy", "Ops"));
        let shop = &p.connections[2];
        assert_eq!((shop.config.engine, shop.config.port, shop.config.tls, shop.group.as_deref()), (Engine::Mysql, Some(3307), Tls::Require, Some("Web")));
        assert_eq!(p.connections[3].notes.len(), 1, "relative SQLite path is flagged");
        assert_eq!(p.skipped.len(), 1);
        let dump = format!("{p:?}");
        assert!(!dump.contains("plaintext!") && !dump.contains("hunter2"), "passwords never come over");
    }

    #[test]
    fn workspace_files_keep_settings_under_a_key() {
        let p = parse_text(
            r#"{ "folders": [], "settings": { "sqltools.connections": [ { "name": "pg", "driver": "PostgreSQL", "server": "h", "database": "d", "username": "u", "pgOptions": { "enableSsl": "Disabled" } } ] } }"#,
            "/w/team.code-workspace",
        );
        assert_eq!(p.label, "VS Code workspace");
        assert_eq!(p.connections[0].config.tls, Tls::Disable);
    }
}
