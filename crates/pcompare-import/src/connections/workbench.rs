//! MySQL Workbench `connections.xml` (a GRT object dump).
//!
//! ```xml
//! <data grt_format="2.0">
//!   <value type="list" content-type="object" content-struct-name="db.mgmt.Connection">
//!     <value type="object" struct-name="db.mgmt.Connection" id="…">
//!       <link type="object" struct-name="db.mgmt.Driver" key="driver">com.mysql.rdbms.mysql.driver.native</link>
//!       <value type="dict" key="parameterValues">
//!         <value type="string" key="hostName">127.0.0.1</value>
//!         <value type="int" key="port">3306</value>
//!         <value type="string" key="userName">root</value>
//!         <value type="string" key="schema">shop</value>
//!         <value type="int" key="useSSL">1</value>
//!       </value>
//!       <value type="string" key="name">Local instance</value>
//!     </value>
//!   </value>
//! </data>
//! ```
//!
//! Workbench names folders with a `/` in the connection name ("Prod/Shop db"),
//! one level deep. `password` / `sshPassword` can be in plain text there and
//! are ignored.

use super::{ConnectionDraft, Engine, ImportedConnection, Parsed, Tls};
use crate::{Result, Skipped};
use roxmltree::Node;
use std::path::PathBuf;

fn keyed<'a, 'i>(node: Node<'a, 'i>, key: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| c.is_element() && c.attribute("key") == Some(key))
}

fn keyed_text<'a>(node: Node<'a, '_>, key: &str) -> &'a str {
    keyed(node, key).and_then(|n| n.text()).map(str::trim).unwrap_or_default()
}

pub(crate) fn parse(root: Node) -> Result<Parsed> {
    let mut out = Parsed {
        label: "MySQL Workbench".into(),
        ..Default::default()
    };
    let conns = root
        .descendants()
        .filter(|n| n.has_tag_name("value") && n.attribute("struct-name") == Some("db.mgmt.Connection"));
    for conn in conns {
        let full_name = keyed_text(conn, "name");
        let (group, name) = match full_name.split_once('/') {
            Some((g, n)) if !g.trim().is_empty() && !n.trim().is_empty() => (Some(g.trim().to_owned()), n.trim()),
            _ => (None, full_name),
        };
        let driver = keyed_text(conn, "driver");
        let Some(params) = keyed(conn, "parameterValues") else {
            continue;
        };
        // Older files use `host` / `username`.
        let p = |k: &str| match (keyed_text(params, k), k) {
            ("", "hostName") => keyed_text(params, "host"),
            ("", "userName") => keyed_text(params, "username"),
            (v, _) => v,
        };
        let mut cfg = ConnectionDraft::new(Engine::Mysql);
        let mut notes = Vec::new();
        if driver.ends_with("_sshtun") {
            out.skipped.push(Skipped {
                name: full_name.to_owned(),
                reason: "connects through an SSH tunnel, which PCompare doesn't support yet".into(),
            });
            continue;
        }
        if driver.ends_with("_socket") {
            notes.push("Connected through a local socket or pipe; PCompare uses TCP on localhost".into());
            cfg.host = "localhost".into();
        } else if driver.contains(".ldap") {
            notes.push("Signs in through LDAP or Kerberos, which PCompare may not support; use a regular MySQL login if it fails".into());
            cfg.host = match p("hostName") {
                "" => "localhost".into(),
                h => h.to_owned(),
            };
        } else if !driver.is_empty() && !driver.starts_with("com.mysql.rdbms.mysql.driver") {
            out.skipped.push(Skipped {
                name: full_name.to_owned(),
                reason: format!("uses the `{driver}` driver, not MySQL"),
            });
            continue;
        } else {
            cfg.host = match p("hostName") {
                "" => "localhost".into(),
                h => h.to_owned(),
            };
        }
        cfg.port = p("port").parse().ok().filter(|&port| port != 0);
        cfg.user = p("userName").to_owned();
        cfg.database = p("schema").to_owned();
        // 0 = no SSL, 1 = if available, 2 = required, 3 = verify CA, 4 = verify identity.
        cfg.tls = match p("useSSL") {
            "0" => Tls::Disable,
            "2" | "3" | "4" => Tls::Require,
            _ => Tls::Prefer,
        };
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

/// `connections.xml` per OS: `%APPDATA%\MySQL\Workbench`, macOS
/// `~/Library/Application Support/MySQL/Workbench`, Linux `~/.mysql/workbench`.
pub(crate) fn locations(appdata: Option<PathBuf>, home: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut dirs = Vec::new();
    if let Some(a) = appdata {
        dirs.push(a.join("MySQL").join("Workbench"));
    }
    if let Some(h) = home {
        dirs.push(h.join("Library").join("Application Support").join("MySQL").join("Workbench"));
        dirs.push(h.join(".mysql").join("workbench"));
    }
    dirs.into_iter()
        .map(|d| ("MySQL Workbench".to_owned(), d.join("connections.xml")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<data grt_format="2.0">
  <value _ptr_="0x1" type="list" content-type="object" content-struct-name="db.mgmt.Connection">
    <value type="object" struct-name="db.mgmt.Connection" id="a1">
      <link type="object" struct-name="db.mgmt.Driver" key="driver">com.mysql.rdbms.mysql.driver.native</link>
      <value type="string" key="hostIdentifier">Mysql@127.0.0.1:3306</value>
      <value _ptr_="0x2" type="dict" key="parameterValues">
        <value type="string" key="hostName">127.0.0.1</value>
        <value type="string" key="password"></value>
        <value type="int" key="port">3307</value>
        <value type="string" key="schema">shop</value>
        <value type="int" key="useSSL">2</value>
        <value type="string" key="userName">root</value>
      </value>
      <value type="string" key="name">Prod/Shop DB</value>
    </value>
    <value type="object" struct-name="db.mgmt.Connection" id="a2">
      <link type="object" struct-name="db.mgmt.Driver" key="driver">com.mysql.rdbms.mysql.driver.native_sshtun</link>
      <value _ptr_="0x3" type="dict" key="parameterValues">
        <value type="string" key="hostName">10.0.0.5</value>
      </value>
      <value type="string" key="name">Behind bastion</value>
    </value>
    <value type="object" struct-name="db.mgmt.Connection" id="a3">
      <link type="object" struct-name="db.mgmt.Driver" key="driver">com.mysql.rdbms.mysql.driver.native_socket</link>
      <value _ptr_="0x4" type="dict" key="parameterValues">
        <value type="string" key="socket">/tmp/mysql.sock</value>
        <value type="int" key="useSSL">0</value>
      </value>
      <value type="string" key="name">Local socket</value>
    </value>
  </value>
</data>"#;

    #[test]
    fn reads_tcp_and_socket_connections_and_skips_ssh() {
        let doc = roxmltree::Document::parse(SAMPLE).unwrap();
        let p = parse(doc.root_element()).unwrap();
        assert_eq!(p.connections.len(), 2);
        let c = &p.connections[0];
        assert_eq!((c.name.as_str(), c.group.as_deref()), ("Shop DB", Some("Prod")));
        let cfg = &c.config;
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.user.as_str(), cfg.database.as_str()), ("127.0.0.1", Some(3307), "root", "shop"));
        assert_eq!(cfg.tls, Tls::Require);
        let sock = &p.connections[1];
        assert_eq!((sock.config.host.as_str(), sock.config.tls, sock.notes.len()), ("localhost", Tls::Disable, 1));
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].reason.contains("SSH"));
    }
}
