//! SQL Server Management Studio: the Connect dialog's recent servers and the
//! Registered Servers tree.
//!
//! **Recent servers** (SSMS 18–21 classic dialog), `%APPDATA%\Microsoft\SQL
//! Server Management Studio\<ver>.0\UserSettings.xml`:
//!
//! ```text
//! SqlStudio/SSMS/ConnectionOptions/ServerTypes/Element
//!   Key/guid                      8c91a03d-… = Database Engine
//!   Value/ServerTypeItem/Servers/Element
//!     Time/long                   negated .NET ticks: lower = more recent
//!     Item/ServerConnectionItem/Connections/Element/Item/ServerConnectionSettings
//!       Instance, UserName, AuthenticationMethod, Database,
//!       Advanced/Element{Key/string, Value/string}   IniDb, EC (encrypt), TSC (trust cert)
//! ```
//!
//! Passwords aren't in this file (SSMS keeps them in Credential Manager).
//! SSMS 17 and older use a binary `SqlStudio.bin`, and the SSMS 21/22 modern
//! dialog encrypts whole entries, password included, so neither is read.
//!
//! **Registered Servers** (`RegSrvr*.xml`, and `.regsrvr` exports in the same
//! format): an SML-IF model with one document per group and server, linked
//! by `Parent/Reference/Uri`. A server's settings are an ADO.NET connection
//! string whose password (if saved) is encrypted; the password is ignored.

use super::conn_string;
use super::{sql_server_address, sql_server_tls, Auth, ConnectionDraft, Encrypt, Engine, ImportedConnection, Parsed};
use crate::{format_err, Result, Skipped};
use roxmltree::Node;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The Database Engine's server-type id; the other types are Analysis,
/// Reporting and Integration Services.
const DATABASE_ENGINE: &str = "8c91a03d-f9b4-46c0-a305-b5dcc79ff907";
const REGSRVR_NS: &str = "http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08";
/// Built-in top-level groups of the Registered Servers tree; they aren't
/// folders the user made.
const ROOT_GROUPS: &[&str] = &[
    "DatabaseEngineServerGroup",
    "CentralManagementServerGroup",
    "AnalysisServicesServerGroup",
    "ReportingServicesServerGroup",
    "IntegrationServicesServerGroup",
    "SqlServerCompactEditionServerGroup",
];
/// Newer SMO keeps recent connections in this group; they aren't registered servers.
const MRU_GROUP: &str = "MruSqlConnectionsGroup";

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a, 'i>(node: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    node.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn path<'a, 'i>(mut node: Node<'a, 'i>, names: &[&str]) -> Option<Node<'a, 'i>> {
    for n in names {
        node = child(node, n)?;
    }
    Some(node)
}

fn text<'a>(node: Option<Node<'a, '_>>) -> &'a str {
    node.and_then(|n| n.text()).map(str::trim).unwrap_or_default()
}

/// `<Time><long>-637962559550382281</long></Time>`; lower = used more recently.
fn time(element: Node) -> i64 {
    text(path(element, &["Time", "long"])).parse().unwrap_or(0)
}

/// What a sign-in method number means here: `Ok` for the two PCompare
/// supports, `Err(note)` for Microsoft Entra ID and anything newer.
fn auth_method(n: &str) -> std::result::Result<Auth, String> {
    match n.trim() {
        "0" | "" => Ok(Auth::Integrated),
        "1" => Ok(Auth::Password),
        "2" | "3" | "4" | "5" => Err("Signs in with Microsoft Entra ID, which PCompare doesn't support yet; enter a SQL login instead".into()),
        other => Err(format!("Uses SSMS sign-in method {other}, which PCompare doesn't support; enter a SQL login instead")),
    }
}

// ---------- recent servers (UserSettings.xml) ----------

/// "SQL Server Management Studio 20" from the `20.0` folder the file sits in.
fn version_label(path: &Path) -> (String, Option<u32>) {
    let major = path
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|d| d.to_str())
        .and_then(|d| d.strip_suffix(".0"))
        .and_then(|v| v.parse::<u32>().ok());
    match major {
        Some(v) => (format!("SQL Server Management Studio {v}"), Some(v)),
        None => ("SQL Server Management Studio".into(), None),
    }
}

pub(crate) fn parse_user_settings(root: Node, file: &Path) -> Result<Parsed> {
    let (label, major) = version_label(file);
    let mut out = Parsed {
        label,
        ..Default::default()
    };
    // SqlClient 5 (SSMS 20+) encrypts unless told otherwise; older SSMS didn't.
    let default_encrypt = if major.is_some_and(|v| v >= 20) { Encrypt::Mandatory } else { Encrypt::Optional };
    let Some(types) = path(root, &["SSMS", "ConnectionOptions", "ServerTypes"]) else {
        return Ok(out);
    };
    for server_type in children(types, "Element") {
        let type_id = text(path(server_type, &["Key", "guid"]));
        if !type_id.eq_ignore_ascii_case(DATABASE_ENGINE) {
            continue; // Analysis / Reporting / Integration Services
        }
        let Some(servers) = path(server_type, &["Value", "ServerTypeItem", "Servers"]) else {
            continue;
        };
        let mut servers: Vec<Node> = children(servers, "Element").collect();
        servers.sort_by_key(|e| time(*e));
        for server in servers {
            let Some(item) = path(server, &["Item", "ServerConnectionItem"]) else {
                continue;
            };
            let mut logins: Vec<Node> = child(item, "Connections")
                .map(|c| children(c, "Element").collect())
                .unwrap_or_default();
            logins.sort_by_key(|e| time(*e));
            let settings: Vec<Node> = logins
                .iter()
                .filter_map(|e| path(*e, &["Item", "ServerConnectionSettings"]))
                .collect();
            let several = settings.len() > 1;
            for s in settings {
                let instance = match text(child(s, "Instance")) {
                    "" => text(child(item, "Instance")),
                    i => i,
                };
                match recent_connection(s, instance, default_encrypt, several) {
                    Ok(c) => out.connections.push(c),
                    Err(reason) => out.skipped.push(Skipped {
                        name: instance.to_owned(),
                        reason,
                    }),
                }
            }
        }
    }
    Ok(out)
}

fn recent_connection(
    s: Node,
    instance: &str,
    default_encrypt: Encrypt,
    several_logins: bool,
) -> std::result::Result<ImportedConnection, String> {
    let advanced: HashMap<&str, &str> = child(s, "Advanced")
        .map(|a| {
            children(a, "Element")
                .map(|e| (text(path(e, &["Key", "string"])), text(path(e, &["Value", "string"]))))
                .collect()
        })
        .unwrap_or_default();
    let mut cfg = ConnectionDraft::new(Engine::Mssql);
    let mut notes = Vec::new();
    let addr = sql_server_address(instance)?;
    (cfg.host, cfg.instance, cfg.port) = (addr.host, addr.instance, addr.port);
    notes.extend(addr.notes);
    match auth_method(text(child(s, "AuthenticationMethod"))) {
        Ok(a) => cfg.auth = a,
        Err(note) => notes.push(note),
    }
    if cfg.auth == Auth::Password {
        cfg.user = text(child(s, "UserName")).to_owned();
    }
    let database = match text(child(s, "Database")) {
        "" => advanced.get("IniDb").copied().unwrap_or_default(),
        d => d,
    };
    if !database.eq_ignore_ascii_case("<default>") {
        cfg.database = database.to_owned();
    }
    let encrypt = advanced.get("EC").and_then(|v| Encrypt::parse(v)).unwrap_or(default_encrypt);
    let trust = advanced.get("TSC").is_some_and(|v| super::truthy(v));
    sql_server_tls(encrypt, trust, &mut cfg);
    // Several logins for one server: tell them apart by login.
    let name = match (several_logins, cfg.auth) {
        (false, _) => instance.to_owned(),
        (true, Auth::Integrated) => format!("{instance} (Windows)"),
        (true, Auth::Password) => format!("{instance} ({})", if cfg.user.is_empty() { "SQL login" } else { &cfg.user }),
    };
    Ok(ImportedConnection {
        name,
        group: None,
        config: cfg,
        notes,
    })
}

// ---------- Registered Servers (RegSrvr*.xml / .regsrvr) ----------

pub(crate) fn is_registered_servers(root: Node) -> bool {
    root.descendants().any(|n| n.tag_name().namespace() == Some(REGSRVR_NS))
}

/// One group or server document of the model.
struct Doc<'a, 'i> {
    node: Node<'a, 'i>,
    server: bool,
}

fn reg<'a>(node: Node<'a, '_>, name: &str) -> &'a str {
    text(
        node.children()
            .find(|c| c.tag_name().name() == name && c.tag_name().namespace() == Some(REGSRVR_NS)),
    )
}

fn parent_uri<'a>(node: Node<'a, '_>) -> &'a str {
    node.children()
        .find(|c| c.tag_name().name() == "Parent" && c.tag_name().namespace() == Some(REGSRVR_NS))
        .and_then(|p| p.descendants().find(|d| d.tag_name().name() == "Uri"))
        .and_then(|u| u.text())
        .map(str::trim)
        .unwrap_or_default()
}

pub(crate) fn parse_registered_servers(root: Node, file: &Path) -> Result<Parsed> {
    let export = file.extension().is_some_and(|x| x.eq_ignore_ascii_case("regsrvr"));
    let mut out = Parsed {
        label: if export { "Registered Servers export".into() } else { "SSMS Registered Servers".into() },
        ..Default::default()
    };
    // alias (the document's own URI) → document.
    let mut docs: HashMap<&str, Doc> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for document in root.descendants().filter(|n| n.has_tag_name("document")) {
        let alias = document
            .descendants()
            .find(|n| n.has_tag_name("alias"))
            .and_then(|n| n.text())
            .map(str::trim)
            .unwrap_or_default();
        let Some(node) = document.descendants().find(|n| {
            n.tag_name().namespace() == Some(REGSRVR_NS)
                && matches!(n.tag_name().name(), "RegisteredServer" | "ServerGroup")
        }) else {
            continue;
        };
        let server = node.tag_name().name() == "RegisteredServer";
        docs.insert(alias, Doc { node, server });
        order.push(alias);
    }
    if docs.is_empty() {
        return Err(format_err("this Registered Servers file has no servers"));
    }
    for alias in order {
        let doc = &docs[alias];
        if !doc.server {
            continue;
        }
        let node = doc.node;
        let name = reg(node, "Name");
        let server_type = reg(node, "ServerType");
        if !server_type.is_empty() && server_type != "DatabaseEngine" {
            continue; // Analysis / Reporting / Integration Services
        }
        // Folder names from the parent chain (aliases are escaped, names aren't).
        let mut group = Vec::new();
        let mut up = parent_uri(node);
        let mut mru = false;
        let mut hops = 0;
        while let Some(parent) = docs.get(up).filter(|_| hops < 64) {
            let gname = reg(parent.node, "Name");
            mru |= gname == MRU_GROUP;
            if ROOT_GROUPS.contains(&gname) || gname == MRU_GROUP {
                break;
            }
            group.push(gname);
            up = parent_uri(parent.node);
            hops += 1;
        }
        // A missing parent (the store root, or an export's outer group) ends the walk.
        mru |= up.contains(MRU_GROUP);
        if mru {
            continue;
        }
        group.reverse();
        match registered_server(node) {
            Ok((cfg, notes)) => out.connections.push(ImportedConnection {
                name: if name.is_empty() { cfg.label() } else { name.to_owned() },
                group: Some(group.join("/")).filter(|g| !g.is_empty()),
                config: cfg,
                notes,
            }),
            Err(reason) => out.skipped.push(Skipped {
                name: name.to_owned(),
                reason,
            }),
        }
    }
    Ok(out)
}

fn registered_server(node: Node) -> std::result::Result<(ConnectionDraft, Vec<String>), String> {
    let mut cs = reg(node, "ConnectionStringWithEncryptedPassword").to_owned();
    let server_name = reg(node, "ServerName");
    if cs.is_empty() {
        cs = format!("data source={server_name}");
    }
    let other = reg(node, "OtherParams");
    if !other.is_empty() {
        cs = format!("{cs};{other}");
    }
    let mut parsed = conn_string::parse(&cs, Some(Engine::Mssql))?;
    if parsed.config.host.is_empty() && !server_name.is_empty() {
        super::apply_sql_server(server_name, &mut parsed.config, &mut parsed.notes)?;
    }
    // Without `integrated security` / `authentication` in the string, the
    // stored sign-in method decides.
    let lower = cs.to_ascii_lowercase();
    let explicit = lower.contains("integrated security") || lower.contains("authentication");
    if !explicit {
        match reg(node, "AuthenticationType") {
            "" if parsed.config.user.is_empty() => parsed.config.auth = Auth::Integrated,
            "" => parsed.config.auth = Auth::Password,
            n => match auth_method(n) {
                Ok(a) => parsed.config.auth = a,
                Err(note) => parsed.notes.push(note),
            },
        }
    }
    if parsed.config.auth == Auth::Integrated {
        parsed.config.user.clear();
    }
    // The (encrypted) password is never used.
    Ok((parsed.config, parsed.notes))
}

// ---------- locations ----------

/// Where SSMS keeps these files for the current user, newest first.
pub(crate) fn locations(appdata: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let Some(appdata) = appdata else {
        return Vec::new();
    };
    let ms = appdata.join("Microsoft");
    let studio = ms.join("SQL Server Management Studio");
    let mut out = Vec::new();
    for v in (18..=30).rev() {
        out.push((format!("SQL Server Management Studio {v}"), studio.join(format!("{v}.0")).join("UserSettings.xml")));
    }
    // Registered Servers: only the newest file counts (SSMS copies the older
    // ones forward on upgrade, so the rest are stale).
    let reg = ["RegSrvr17.xml", "RegSrvr16.xml", "RegSrvr.xml"]
        .iter()
        .map(|f| studio.join(f))
        .chain([140, 130, 120, 110, 100].iter().map(|v| {
            ms.join("Microsoft SQL Server").join(v.to_string()).join("Tools").join("Shell").join("RegSrvr.xml")
        }))
        .find(|p| p.is_file());
    if let Some(p) = reg {
        out.push(("SSMS Registered Servers".into(), p));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::Tls;

    fn settings(advanced: &str, auth: &str, user: &str, instance: &str, time: i64) -> String {
        format!(
            r#"<Element><Time><long>{time}</long></Time><Item><ServerConnectionSettings>
              <Password /><Instance>{instance}</Instance><UserName>{user}</UserName>
              <ServerType>8c91a03d-f9b4-46c0-a305-b5dcc79ff907</ServerType>
              <AuthenticationMethod>{auth}</AuthenticationMethod><Database />
              <Advanced>{advanced}</Advanced><OtherParams />
            </ServerConnectionSettings></Item></Element>"#
        )
    }

    fn adv(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(k, v)| format!("<Element><Key><string>{k}</string></Key><Value><string>{v}</string></Value></Element>"))
            .collect()
    }

    fn server(instance: &str, time: i64, logins: &[String]) -> String {
        format!(
            r#"<Element><Time><long>{time}</long></Time><Item><ServerConnectionItem>
              <Instance>{instance}</Instance><AuthenticationMethod>0</AuthenticationMethod>
              <Connections>{}</Connections></ServerConnectionItem></Item></Element>"#,
            logins.concat()
        )
    }

    fn user_settings(servers: &[String]) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<SqlStudio xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><SSMS><ConnectionOptions>
  <LastServerType>8c91a03d-f9b4-46c0-a305-b5dcc79ff907</LastServerType>
  <ServerTypes>
    <Element><Key><guid>8c91a03d-f9b4-46c0-a305-b5dcc79ff907</guid></Key>
      <Value><ServerTypeItem><Servers>{}</Servers></ServerTypeItem></Value></Element>
    <Element><Key><guid>f3747e57-0000-0000-0000-000000000000</guid></Key>
      <Value><ServerTypeItem><Servers>{}</Servers></ServerTypeItem></Value></Element>
  </ServerTypes>
</ConnectionOptions></SSMS></SqlStudio>"#,
            servers.concat(),
            server("ssas01", -9, &[settings("", "0", "", "ssas01", -9)]),
        )
    }

    #[test]
    fn recent_servers_most_recent_first_with_logins_apart() {
        let xml = user_settings(&[
            server(
                r"db01\SQLEXPRESS",
                -100,
                &[settings(&adv(&[("IniDb", "Shop"), ("EC", "True"), ("TSC", "False")]), "1", "app", r"db01\SQLEXPRESS", -100)],
            ),
            server(
                "prod-sql01,1500",
                -200,
                &[
                    settings(&adv(&[("EC", "False")]), "0", "", "prod-sql01,1500", -150),
                    settings(&adv(&[("EC", "Strict")]), "1", "reader", "prod-sql01,1500", -200),
                ],
            ),
            server(r"(localdb)\MSSQLLocalDB", -50, &[settings("", "0", "", r"(localdb)\MSSQLLocalDB", -50)]),
            server("cloud.database.windows.net", -10, &[settings("", "5", "me@corp.com", "cloud.database.windows.net", -10)]),
        ]);
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let file = PathBuf::from("SQL Server Management Studio").join("19.0").join("UserSettings.xml");
        let p = parse_user_settings(doc.root_element(), &file).unwrap();
        assert_eq!(p.label, "SQL Server Management Studio 19");
        let names: Vec<_> = p.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["prod-sql01,1500 (reader)", "prod-sql01,1500 (Windows)", r"db01\SQLEXPRESS", "cloud.database.windows.net"]
        );
        let reader = &p.connections[0].config;
        assert_eq!((reader.port, reader.user.as_str(), reader.tls, reader.trust_server_certificate), (Some(1500), "reader", Tls::Require, false));
        let windows = &p.connections[1].config;
        assert_eq!((windows.auth, windows.tls, windows.trust_server_certificate), (Auth::Integrated, Tls::Prefer, true));
        let express = &p.connections[2].config;
        assert_eq!((express.instance.as_str(), express.database.as_str(), express.tls), ("SQLEXPRESS", "Shop", Tls::Require));
        assert_eq!(p.connections[3].notes.len(), 1, "Entra sign-in is flagged");
        assert_eq!(p.skipped.len(), 1, "LocalDB is skipped");
    }

    #[test]
    fn version_from_the_folder_sets_the_encrypt_default() {
        let xml = user_settings(&[server("srv", -1, &[settings("", "0", "", "srv", -1)])]);
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let dir = PathBuf::from("SQL Server Management Studio").join("20.0").join("UserSettings.xml");
        let p = parse_user_settings(doc.root_element(), &dir).unwrap();
        assert_eq!(p.label, "SQL Server Management Studio 20");
        assert_eq!(p.connections[0].config.tls, Tls::Require, "SSMS 20 encrypts by default");
    }

    const REGSRVR: &str = r#"<?xml version="1.0"?>
<model xmlns="http://schemas.serviceml.org/sml/2007/02">
  <name>RegisteredServersStore</name>
  <xs:bufferSchema xmlns:xs="http://www.w3.org/2001/XMLSchema" />
  <definitions xmlns:sfc="http://schemas.microsoft.com/sqlserver/sfc/serialization/2007/08">
    <document><docinfo><aliases><alias>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup</alias></aliases><sfc:version DomainVersion="1" /></docinfo>
      <data><RegisteredServers:ServerGroup xmlns:RegisteredServers="http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08" xmlns:sfc="http://schemas.microsoft.com/sqlserver/sfc/serialization/2007/08" xmlns:sml="http://schemas.serviceml.org/sml/2007/02">
        <RegisteredServers:Parent><sfc:Reference sml:ref="true"><sml:Uri>/RegisteredServersStore</sml:Uri></sfc:Reference></RegisteredServers:Parent>
        <RegisteredServers:Name type="string">DatabaseEngineServerGroup</RegisteredServers:Name>
        <RegisteredServers:ServerType type="ServerType">DatabaseEngine</RegisteredServers:ServerType>
      </RegisteredServers:ServerGroup></data></document>
    <document><docinfo><aliases><alias>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/ServerGroup/Prod_.EU</alias></aliases></docinfo>
      <data><RegisteredServers:ServerGroup xmlns:RegisteredServers="http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08" xmlns:sfc="http://schemas.microsoft.com/sqlserver/sfc/serialization/2007/08" xmlns:sml="http://schemas.serviceml.org/sml/2007/02">
        <RegisteredServers:Parent><sfc:Reference sml:ref="true"><sml:Uri>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup</sml:Uri></sfc:Reference></RegisteredServers:Parent>
        <RegisteredServers:Name type="string">Prod.EU</RegisteredServers:Name>
        <RegisteredServers:ServerType type="ServerType">DatabaseEngine</RegisteredServers:ServerType>
      </RegisteredServers:ServerGroup></data></document>
    <document><docinfo><aliases><alias>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/ServerGroup/Prod_.EU/RegisteredServer/orders</alias></aliases></docinfo>
      <data><RegisteredServers:RegisteredServer xmlns:RegisteredServers="http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08" xmlns:sfc="http://schemas.microsoft.com/sqlserver/sfc/serialization/2007/08" xmlns:sml="http://schemas.serviceml.org/sml/2007/02">
        <RegisteredServers:Parent><sfc:Reference sml:ref="true"><sml:Uri>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/ServerGroup/Prod_.EU</sml:Uri></sfc:Reference></RegisteredServers:Parent>
        <RegisteredServers:Name type="string">Orders</RegisteredServers:Name>
        <RegisteredServers:ServerName type="string">sql01\PROD</RegisteredServers:ServerName>
        <RegisteredServers:ServerType type="ServerType">DatabaseEngine</RegisteredServers:ServerType>
        <RegisteredServers:ConnectionStringWithEncryptedPassword type="string">data source=sql01\PROD;initial catalog=Orders;integrated security=True;pooling=False;multipleactiveresultsets=False;connect timeout=30;encrypt=False;trustservercertificate=False;packet size=4096;application name="Microsoft SQL Server Management Studio"</RegisteredServers:ConnectionStringWithEncryptedPassword>
        <RegisteredServers:OtherParams type="string">ApplicationIntent=ReadOnly</RegisteredServers:OtherParams>
        <RegisteredServers:AuthenticationType type="int">0</RegisteredServers:AuthenticationType>
      </RegisteredServers:RegisteredServer></data></document>
    <document><docinfo><aliases><alias>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/RegisteredServer/reports</alias></aliases></docinfo>
      <data><RegisteredServers:RegisteredServer xmlns:RegisteredServers="http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08" xmlns:sfc="http://schemas.microsoft.com/sqlserver/sfc/serialization/2007/08" xmlns:sml="http://schemas.serviceml.org/sml/2007/02">
        <RegisteredServers:Parent><sfc:Reference sml:ref="true"><sml:Uri>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup</sml:Uri></sfc:Reference></RegisteredServers:Parent>
        <RegisteredServers:Name type="string">Reports</RegisteredServers:Name>
        <RegisteredServers:ServerName type="string">rpt01,1444</RegisteredServers:ServerName>
        <RegisteredServers:ServerType type="ServerType">DatabaseEngine</RegisteredServers:ServerType>
        <RegisteredServers:ConnectionStringWithEncryptedPassword type="string">data source="rpt01,1444";user id=report;password=AQAAANCMnd8BFdERjHoAwE/Cl+sBAAAA;multiple active result sets=False;encrypt=True;trust server certificate=True</RegisteredServers:ConnectionStringWithEncryptedPassword>
        <RegisteredServers:AuthenticationType type="int">1</RegisteredServers:AuthenticationType>
      </RegisteredServers:RegisteredServer></data></document>
    <document><docinfo><aliases><alias>/RegisteredServersStore/ServerGroup/AnalysisServicesServerGroup/RegisteredServer/cube</alias></aliases></docinfo>
      <data><RegisteredServers:RegisteredServer xmlns:RegisteredServers="http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08">
        <RegisteredServers:Name type="string">cube</RegisteredServers:Name>
        <RegisteredServers:ServerType type="ServerType">AnalysisServices</RegisteredServers:ServerType>
      </RegisteredServers:RegisteredServer></data></document>
  </definitions>
</model>"#;

    #[test]
    fn registered_servers_keep_their_groups_and_drop_passwords() {
        let doc = roxmltree::Document::parse(REGSRVR).unwrap();
        assert!(is_registered_servers(doc.root_element()));
        let p = parse_registered_servers(doc.root_element(), Path::new("prod.regsrvr")).unwrap();
        assert_eq!(p.label, "Registered Servers export");
        assert_eq!(p.connections.len(), 2, "Analysis Services is left out");
        let orders = &p.connections[0];
        assert_eq!((orders.name.as_str(), orders.group.as_deref()), ("Orders", Some("Prod.EU")));
        let c = &orders.config;
        assert_eq!((c.host.as_str(), c.instance.as_str(), c.database.as_str(), c.auth), ("sql01", "PROD", "Orders", Auth::Integrated));
        assert_eq!((c.tls, c.trust_server_certificate), (Tls::Prefer, true));
        let reports = &p.connections[1];
        assert_eq!(reports.group, None);
        let c = &reports.config;
        assert_eq!((c.host.as_str(), c.port, c.user.as_str(), c.auth), ("rpt01", Some(1444), "report", Auth::Password));
        assert_eq!((c.tls, c.trust_server_certificate), (Tls::Require, true));
        assert!(!format!("{p:?}").contains("AQAAANCM"), "the encrypted password never comes over");
    }
}
