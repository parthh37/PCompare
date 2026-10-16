//! HeidiSQL's exported settings (File → Export settings, or the portable
//! build's `portable_settings.txt`): one `path<|||>type<|||>value` line per
//! setting, sessions under `Servers\<folders…>\<session>\<key>`.
//!
//! ```text
//! Servers\Prod\Folder<|||>3<|||>1
//! Servers\Prod\db1\Host<|||>1<|||>sql01\INST
//! Servers\Prod\db1\NetType<|||>3<|||>4
//! Servers\Prod\db1\Port<|||>1<|||>1433
//! ```
//!
//! The installed Windows build keeps the same tree in the registry; export
//! it to a file first. `Password` (obfuscated, not encrypted) is ignored.

use super::{apply_sql_server, sql_server_tls, Auth, ConnectionDraft, Encrypt, Engine, ImportedConnection, Parsed, Tls};
use crate::Skipped;
use std::collections::BTreeMap;

const SEP: &str = "<|||>";

pub(crate) fn is_settings(text: &str) -> bool {
    text.lines().take(50).any(|l| l.contains(SEP))
}

pub(crate) fn parse(text: &str) -> Parsed {
    let mut out = Parsed {
        label: "HeidiSQL".into(),
        ..Default::default()
    };
    // `Servers\a\b` → its keys, in file order of first appearance.
    let mut nodes: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, SEP);
        let (Some(path), Some(_kind), Some(value)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some(rest) = path.strip_prefix("Servers\\") else {
            continue;
        };
        let Some((node, key)) = rest.rsplit_once('\\') else {
            continue;
        };
        let value = value.replace("<{{{>", "\r").replace("<}}}>", "\n");
        if !nodes.contains_key(node) {
            order.push(node.to_owned());
        }
        nodes.entry(node.to_owned()).or_default().insert(key.to_owned(), value);
    }
    for node in order {
        let keys = &nodes[&node];
        if keys.contains_key("Folder") || !(keys.contains_key("Host") || keys.contains_key("NetType")) {
            continue;
        }
        let (group, name) = match node.rsplit_once('\\') {
            Some((g, n)) => (Some(g.replace('\\', "/")), n.to_owned()),
            None => (None, node.clone()),
        };
        match session(keys) {
            Ok((config, notes)) => out.connections.push(ImportedConnection {
                name,
                group,
                config,
                notes,
            }),
            Err(reason) => out.skipped.push(Skipped { name, reason }),
        }
    }
    out
}

fn session(k: &BTreeMap<String, String>) -> std::result::Result<(ConnectionDraft, Vec<String>), String> {
    let get = |key: &str| k.get(key).map(|v| v.trim()).unwrap_or_default();
    let on = |key: &str| get(key) == "1";
    let mut notes = Vec::new();
    let net: u32 = get("NetType").parse().unwrap_or(0);
    let engine = match net {
        0 | 1 | 16 => Engine::Mysql,
        3..=7 => Engine::Mssql,
        8 => Engine::Postgres,
        10 => Engine::Sqlite,
        2 | 9 => return Err("connects through an SSH tunnel, which PCompare doesn't support yet".into()),
        17 => return Err("encrypted SQLite databases can't be opened".into()),
        11 => return Err("ProxySQL admin isn't a database PCompare supports".into()),
        _ => return Err("Interbase / Firebird isn't a database PCompare supports".into()),
    };
    let mut cfg = ConnectionDraft::new(engine);
    let host = get("Host");
    if engine == Engine::Sqlite {
        // Several attached files are `|`-separated; the first is the main one.
        let mut files = host.split('|').map(str::trim).filter(|f| !f.is_empty());
        cfg.file = files.next().ok_or("no database file")?.to_owned();
        if files.next().is_some() {
            notes.push("Had more databases attached; PCompare opens the first file".into());
        }
        return Ok((cfg, notes));
    }
    let databases: Vec<&str> = get("Databases").split(';').map(str::trim).filter(|d| !d.is_empty()).collect();
    if let Some(first) = databases.first() {
        cfg.database = (*first).to_owned();
    }
    if databases.len() > 1 {
        notes.push(format!("Listed {} databases; pick the others in the database list", databases.len()));
    }
    match engine {
        Engine::Mssql => {
            if net != 4 {
                notes.push("Connects over TCP; PCompare can't use named pipes or the other legacy protocols".into());
            }
            // HeidiSQL connects to `host[\instance],port`.
            let port = get("Port");
            let server = if port.is_empty() || net != 4 { host.to_owned() } else { format!("{host},{port}") };
            apply_sql_server(&server, &mut cfg, &mut notes)?;
            if on("WindowsAuth") {
                cfg.auth = Auth::Integrated;
            } else {
                cfg.user = get("User").to_owned();
            }
            // HeidiSQL has no encryption options for SQL Server.
            sql_server_tls(if on("SSL_Active") { Encrypt::Mandatory } else { Encrypt::Optional }, true, &mut cfg);
        }
        _ => {
            if net == 1 {
                notes.push("Connected through a named pipe; PCompare uses TCP on localhost".into());
            }
            cfg.host = if host.is_empty() || net == 1 { "localhost".into() } else { host.to_owned() };
            cfg.port = get("Port").parse().ok().filter(|&p| p != 0);
            cfg.user = get("User").to_owned();
            cfg.tls = if on("SSL_Active") { Tls::Require } else { Tls::Prefer };
        }
    }
    Ok((cfg, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sessions_in_nested_folders() {
        let text = [
            r"Servers\Prod\Folder<|||>3<|||>1",
            r"Servers\Prod\EU\Folder<|||>3<|||>1",
            r"Servers\Prod\EU\orders\Host<|||>1<|||>sql01\INST",
            r"Servers\Prod\EU\orders\NetType<|||>3<|||>4",
            r"Servers\Prod\EU\orders\Port<|||>1<|||>1433",
            r"Servers\Prod\EU\orders\WindowsAuth<|||>3<|||>1",
            r"Servers\Prod\EU\orders\Databases<|||>1<|||>Sales;HR",
            r"Servers\Prod\EU\orders\Password<|||>1<|||>7A6B5C4",
            r"Servers\shop\Host<|||>1<|||>db.local",
            r"Servers\shop\NetType<|||>3<|||>0",
            r"Servers\shop\Port<|||>1<|||>3307",
            r"Servers\shop\User<|||>1<|||>root",
            r"Servers\shop\SSL_Active<|||>3<|||>1",
            r"Servers\cache\Host<|||>1<|||>C:\data\a.db|C:\data\b.db",
            r"Servers\cache\NetType<|||>3<|||>10",
            r"Servers\tunnel\Host<|||>1<|||>10.0.0.5",
            r"Servers\tunnel\NetType<|||>3<|||>2",
            r"LastSessions<|||>1<|||>shop",
        ]
        .join("\r\n");
        assert!(is_settings(&text));
        let p = parse(&text);
        let names: Vec<_> = p.connections.iter().map(|c| (c.group.as_deref(), c.name.as_str())).collect();
        assert_eq!(names, [(Some("Prod/EU"), "orders"), (None, "shop"), (None, "cache")]);
        let orders = &p.connections[0];
        let c = &orders.config;
        assert_eq!((c.host.as_str(), c.port, c.auth, c.database.as_str()), ("sql01", Some(1433), Auth::Integrated, "Sales"));
        assert_eq!(orders.notes.len(), 1, "second database is mentioned");
        let shop = &p.connections[1].config;
        assert_eq!((shop.engine, shop.port, shop.user.as_str(), shop.tls), (Engine::Mysql, Some(3307), "root", Tls::Require));
        assert_eq!(p.connections[2].config.file, r"C:\data\a.db");
        assert_eq!(p.skipped.len(), 1);
        assert!(!format!("{p:?}").contains("7A6B5C4"));
    }
}
