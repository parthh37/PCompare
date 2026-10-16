//! DBeaver `data-sources*.json` in `DBeaverData/workspace6/<project>/.dbeaver/`.
//!
//! ```json
//! { "folders": { "Prod": {}, "Prod/EU": {} },
//!   "connections": { "postgres-jdbc-…": {
//!       "provider": "postgresql", "driver": "postgres-jdbc", "name": "PG prod", "folder": "Prod/EU",
//!       "configuration": { "host": "db", "port": "5432", "database": "app", "url": "jdbc:postgresql://…",
//!         "configurationType": "MANUAL", "auth-model": "native",
//!         "handlers": { "postgre_ssl": { "enabled": true, "properties": { "sslMode": "verify-full" } } } } } } }
//! ```
//!
//! DBeaver keeps the login name together with the password in the encrypted
//! `credentials-config.json`, which isn't read, so connections usually come
//! without a login.

use super::conn_string;
use super::{apply_sql_server, sql_server_tls, truthy, Auth, ConnectionDraft, Encrypt, Engine, ImportedConnection, Parsed, Tls};
use crate::Skipped;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) fn is_data_sources(v: &Value) -> bool {
    v.get("connections")
        .and_then(Value::as_object)
        .is_some_and(|c| c.values().any(|x| x.get("provider").is_some() || x.get("driver").is_some()))
}

/// The engine a provider/driver pair is for (current and legacy ids).
fn engine(provider: &str, driver: &str) -> Option<Engine> {
    let d = driver.to_ascii_lowercase();
    Some(match provider.to_ascii_lowercase().as_str() {
        "sqlserver" => Engine::Mssql,
        // The legacy `mssql` provider also carries Sybase drivers.
        "mssql" if !d.contains("sybase") => Engine::Mssql,
        "mysql" => Engine::Mysql,
        "postgresql" => Engine::Postgres,
        "sqlite" => Engine::Sqlite,
        "generic" if d.starts_with("sqlite") => Engine::Sqlite,
        "generic" if d.starts_with("mssql") => Engine::Mssql,
        "generic" if d.starts_with("mysql") => Engine::Mysql,
        "generic" if d.starts_with("postgresql") => Engine::Postgres,
        _ => return None,
    })
}

fn s<'a>(v: Option<&'a Value>, k: &str) -> &'a str {
    v.and_then(|v| v.get(k)).and_then(Value::as_str).map(str::trim).unwrap_or_default()
}

/// An enabled `configuration.handlers.<id>` and its properties.
fn handler<'a>(cfg: Option<&'a Value>, id: &str) -> Option<&'a Value> {
    let h = cfg?.get("handlers")?.get(id)?;
    let on = h.get("enabled").is_some_and(|e| e.as_bool() == Some(true) || e.as_str().is_some_and(truthy));
    on.then(|| h.get("properties").unwrap_or(&Value::Null))
}

pub(crate) fn parse(v: &Value, path: &Path) -> Parsed {
    let project = path
        .parent()
        .filter(|d| d.ends_with(".dbeaver"))
        .and_then(Path::parent)
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("General");
    let mut out = Parsed {
        label: if project == "General" { "DBeaver".into() } else { format!("DBeaver ({project})") },
        ..Default::default()
    };
    let Some(conns) = v.get("connections").and_then(Value::as_object) else {
        return out;
    };
    for c in conns.values() {
        let name = s(Some(c), "name");
        let provider = s(Some(c), "provider");
        let driver = s(Some(c), "driver");
        let group = Some(s(Some(c), "folder").to_owned()).filter(|g| !g.is_empty());
        let result = match engine(provider, driver) {
            Some(e) => connection(e, c.get("configuration")),
            None => Err(format!("{} isn't a database PCompare supports", if provider.is_empty() { driver } else { provider })),
        };
        match result {
            Ok((config, notes)) => out.connections.push(ImportedConnection {
                name: if name.is_empty() { config.label() } else { name.to_owned() },
                group,
                config,
                notes,
            }),
            Err(reason) => out.skipped.push(Skipped {
                name: name.to_owned(),
                reason,
            }),
        }
    }
    out
}

fn connection(engine: Engine, cfg: Option<&Value>) -> std::result::Result<(ConnectionDraft, Vec<String>), String> {
    let mut notes = Vec::new();
    let props = cfg.and_then(|c| c.get("properties"));
    let provider_props = cfg.and_then(|c| c.get("provider-properties"));
    let url = s(cfg, "url");
    let mut draft = if s(cfg, "configurationType").eq_ignore_ascii_case("URL") && !url.is_empty() {
        // The password (if the URL has one) is dropped.
        let p = conn_string::parse(url, Some(engine))?;
        notes.extend(p.notes);
        p.config
    } else {
        let mut d = ConnectionDraft::new(engine);
        if engine == Engine::Sqlite {
            d.file = s(cfg, "database").to_owned();
            if d.file.is_empty() {
                return Err("no database file".into());
            }
            return Ok((d, notes));
        }
        let host = match s(cfg, "host") {
            "" => "localhost",
            h => h,
        };
        if engine == Engine::Mssql {
            let instance = s(props, "instanceName");
            let server = if instance.is_empty() || host.contains('\\') { host.to_owned() } else { format!("{host}\\{instance}") };
            apply_sql_server(&server, &mut d, &mut notes)?;
        } else {
            d.host = host.to_owned();
        }
        d.port = d.port.or_else(|| s(cfg, "port").parse().ok().filter(|&p| p != 0));
        d.database = s(cfg, "database").to_owned();
        d
    };
    // Only hand-edited files carry the login here.
    if draft.user.is_empty() {
        draft.user = s(cfg, "user").to_owned();
    }
    let auth_model = s(cfg, "auth-model");
    match engine {
        Engine::Mssql => {
            let windows = auth_model == "sqlserver_windows"
                || truthy(s(provider_props, "@dbeaver-connection-windows-auth@"))
                || truthy(s(props, "integratedSecurity"));
            if windows {
                draft.auth = Auth::Integrated;
                draft.user.clear();
            } else if !matches!(auth_model, "" | "native" | "sqlserver_database") {
                notes.push(format!("Signs in with DBeaver's `{auth_model}` method, which PCompare doesn't support; enter a SQL login instead"));
            }
            // DBeaver turns encryption off unless its SSL handler or the driver property asks for it.
            let encrypt = if handler(cfg, "mssql_ssl").is_some() {
                Encrypt::Mandatory
            } else {
                Encrypt::parse(s(props, "encrypt")).unwrap_or(Encrypt::Optional)
            };
            let trust = truthy(s(provider_props, "sslTrustServerCertificate")) || truthy(s(props, "trustServerCertificate"));
            sql_server_tls(encrypt, trust, &mut draft);
        }
        Engine::Postgres => {
            if let Some(ssl) = handler(cfg, "postgre_ssl") {
                let mode = match s(Some(ssl), "sslMode") {
                    "" => s(Some(ssl), "ssl.sslMode"),
                    m => m,
                };
                draft.tls = match mode.to_ascii_lowercase().as_str() {
                    "disable" => Tls::Disable,
                    "allow" | "prefer" | "" => Tls::Prefer,
                    _ => Tls::Require,
                };
            }
        }
        Engine::Mysql => {
            if let Some(ssl) = handler(cfg, "mysql_ssl") {
                draft.tls = if truthy(s(Some(ssl), "ssl.require")) || truthy(s(Some(ssl), "ssl.verify.server")) {
                    Tls::Require
                } else {
                    Tls::Prefer
                };
            }
        }
        Engine::Sqlite => {}
    }
    if handler(cfg, "ssh_tunnel").is_some() {
        notes.push("Was reached through an SSH tunnel, which PCompare doesn't support yet".into());
    }
    if draft.user.is_empty() && draft.auth == Auth::Password && engine != Engine::Sqlite {
        notes.push("DBeaver stores the login name with the password, so enter it here or when you edit the connection".into());
    }
    Ok((draft, notes))
}

/// `data-sources*.json` of every project in the DBeaver workspace.
pub(crate) fn locations(appdata: Option<PathBuf>, home: Option<PathBuf>, xdg_data: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(a) = appdata {
        roots.push(a.join("DBeaverData"));
    }
    if let Some(h) = &home {
        roots.push(h.join("Library").join("DBeaverData"));
    }
    if let Some(x) = xdg_data.or_else(|| home.map(|h| h.join(".local").join("share"))) {
        roots.push(x.join("DBeaverData"));
    }
    let mut out = Vec::new();
    for root in roots {
        let projects = std::fs::read_dir(root.join("workspace6")).into_iter().flatten().flatten();
        for project in projects {
            let dir = project.path().join(".dbeaver");
            let files = std::fs::read_dir(&dir).into_iter().flatten().flatten();
            for f in files {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.starts_with("data-sources") && name.ends_with(".json") {
                    out.push(("DBeaver".to_owned(), f.path()));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_engines_folders_and_tls_handlers() {
        let v: Value = serde_json::from_str(
            r#"{
              "folders": { "Prod": {}, "Prod/EU": {} },
              "connections": {
                "pg-1": { "provider": "postgresql", "driver": "postgres-jdbc", "name": "PG prod", "folder": "Prod/EU",
                  "configuration": { "host": "db.example.com", "port": "5432", "database": "appdb", "configurationType": "MANUAL",
                    "auth-model": "native",
                    "handlers": { "postgre_ssl": { "type": "CONFIG", "enabled": true, "properties": { "sslMode": "verify-full" } },
                                  "ssh_tunnel": { "enabled": true, "properties": { "host": "bastion" } } } } },
                "ms-1": { "provider": "sqlserver", "driver": "microsoft", "name": "Orders",
                  "configuration": { "host": "sql01", "port": "1433", "database": "Orders", "auth-model": "sqlserver_windows",
                    "provider-properties": { "sslTrustServerCertificate": "true" } } },
                "my-1": { "provider": "mysql", "driver": "mariaDB", "name": "Maria",
                  "configuration": { "configurationType": "URL", "url": "jdbc:mariadb://maria:3307/shop?password=oops",
                    "handlers": { "mysql_ssl": { "enabled": true, "properties": { "ssl.require": "true" } } } } },
                "lite-1": { "provider": "generic", "driver": "sqlite_jdbc", "name": "cache",
                  "configuration": { "database": "C:\\data\\cache.db", "url": "jdbc:sqlite:C:\\data\\cache.db" } },
                "ora-1": { "provider": "oracle", "driver": "oracle_thin", "name": "HR" }
              }
            }"#,
        )
        .unwrap();
        assert!(is_data_sources(&v));
        let path = PathBuf::from("DBeaverData").join("workspace6").join("General").join(".dbeaver").join("data-sources.json");
        let mut p = parse(&v, &path);
        assert_eq!(p.label, "DBeaver");
        p.connections.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = p.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Maria", "Orders", "PG prod", "cache"]);
        let maria = &p.connections[0].config;
        assert_eq!((maria.engine, maria.port, maria.database.as_str(), maria.tls), (Engine::Mysql, Some(3307), "shop", Tls::Require));
        let orders = &p.connections[1];
        assert_eq!((orders.config.auth, orders.config.tls, orders.config.trust_server_certificate), (Auth::Integrated, Tls::Prefer, true));
        assert!(orders.notes.is_empty());
        let pg = &p.connections[2];
        assert_eq!((pg.group.as_deref(), pg.config.tls), (Some("Prod/EU"), Tls::Require));
        assert_eq!(pg.notes.len(), 2, "SSH tunnel and missing login are flagged");
        assert_eq!(p.connections[3].config.file, r"C:\data\cache.db");
        assert_eq!(p.skipped.len(), 1);
        assert!(!format!("{p:?}").contains("oops"));
    }
}
