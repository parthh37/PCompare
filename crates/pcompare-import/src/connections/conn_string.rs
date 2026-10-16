//! Connection strings in the forms tools store and people paste:
//!
//! - `key=value;` lists: ADO.NET (SQL Server, Npgsql, MySQL Connector),
//!   ODBC (`Driver={…}`) and OLE DB (`Provider=…`, as in `.udl` files)
//! - URLs: `postgres://`, `mysql://`, `mariadb://`, `mssql://`, `sqlserver://`, `sqlite:`
//! - JDBC: `jdbc:sqlserver:`, `jdbc:jtds:sqlserver:`, `jdbc:postgresql:`,
//!   `jdbc:mysql:`, `jdbc:mariadb:`, `jdbc:sqlite:`
//! - libpq keyword strings: `host=… port=… dbname=…`
//!
//! A password in the string is returned on its own so the caller decides
//! whether to use it.

use super::{apply_sql_server, sql_server_tls, truthy, ConnectionDraft, Encrypt, Engine, Auth, Tls};

#[derive(Debug, Default, PartialEq)]
pub struct ParsedString {
    pub config: ConnectionDraft,
    pub password: Option<String>,
    pub notes: Vec<String>,
}

type Res<T> = std::result::Result<T, String>;

/// Parses any supported connection string. `hint` settles strings that fit
/// several engines (e.g. a tool that only stores SQL Server connections).
pub fn parse(text: &str, hint: Option<Engine>) -> Res<ParsedString> {
    let mut s = text.trim();
    // Pasted straight out of source code.
    for q in ['"', '\'', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            s = s[1..s.len() - 1].trim();
        }
    }
    if s.is_empty() {
        return Err("the connection string is empty".into());
    }
    if let Some(rest) = strip_prefix_ci(s, "jdbc:") {
        return jdbc(rest);
    }
    if let Some((scheme, rest)) = url_scheme(s) {
        return url(&scheme, rest);
    }
    if s.contains('=') {
        if looks_like_libpq(s) {
            return libpq(s);
        }
        return key_values(s, hint);
    }
    Err("not a connection string PCompare recognises".into())
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)).then(|| &s[prefix.len()..])
}

// ---------- URLs ----------

/// `scheme:rest` for the schemes this parser knows (lower-cased).
fn url_scheme(s: &str) -> Option<(String, &str)> {
    let (scheme, rest) = s.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    let known = matches!(
        scheme.as_str(),
        "postgres" | "postgresql" | "mysql" | "mariadb" | "mysqlx" | "mssql" | "sqlserver" | "sqlite" | "sqlite3" | "file"
    );
    known.then_some((scheme, rest))
}

fn engine_for_scheme(scheme: &str) -> Option<Engine> {
    Some(match scheme {
        "postgres" | "postgresql" | "pgsql" => Engine::Postgres,
        "mysql" | "mariadb" | "mysqlx" => Engine::Mysql,
        "mssql" | "sqlserver" => Engine::Mssql,
        "sqlite" | "sqlite3" | "file" => Engine::Sqlite,
        _ => return None,
    })
}

/// `scheme:` already removed; `rest` is `//user:pass@host:port/db?query`.
fn url(scheme: &str, rest: &str) -> Res<ParsedString> {
    let engine = engine_for_scheme(scheme).ok_or_else(|| format!("`{scheme}:` databases aren't supported"))?;
    if engine == Engine::Sqlite {
        return sqlite_path(rest);
    }
    if engine == Engine::Mssql && scheme == "sqlserver" && rest.contains(';') {
        // `sqlserver://host;databaseName=x` is the JDBC form without `jdbc:`.
        return sql_server_jdbc(rest.trim_start_matches('/'));
    }
    let mut out = ParsedString {
        config: ConnectionDraft::new(engine),
        ..Default::default()
    };
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = main.split_once('/').unwrap_or((main, ""));
    let (userinfo, hosts) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    if let Some(u) = userinfo {
        let (user, pass) = u.split_once(':').map_or((u, None), |(a, b)| (a, Some(b)));
        out.config.user = percent_decode(user);
        out.password = pass.map(percent_decode);
    }
    let mut hosts = hosts.split(',');
    let first = hosts.next().unwrap_or_default();
    if hosts.next().is_some() {
        out.notes.push("Lists several hosts; PCompare connects to the first".into());
    }
    let (host, port) = split_host_port(first);
    out.config.port = port;
    if engine == Engine::Mssql {
        // `mssql://host\instance` (backslash or percent-encoded).
        let host = percent_decode(host);
        let (h, i) = host.split_once('\\').unwrap_or((&host, ""));
        out.config.host = h.to_owned();
        out.config.instance = if port.is_some() { String::new() } else { i.to_owned() };
    } else {
        out.config.host = percent_decode(host);
    }
    if out.config.host.is_empty() {
        out.config.host = "localhost".into();
    }
    out.config.database = percent_decode(path.trim_end_matches('/'));
    apply_params(&mut out, query.split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_owned(), percent_decode(v))));
    Ok(out)
}

/// `host`, `host:port` or `[v6]:port`.
fn split_host_port(s: &str) -> (&str, Option<u16>) {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').unwrap_or((rest, ""));
        return (h, after.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h, Some(port)),
            Err(_) => (s, None),
        },
        _ => (s, None),
    }
}

/// `sqlite:///abs/path`, `sqlite:relative.db`, `file:C:/x.db?mode=ro`.
fn sqlite_path(rest: &str) -> Res<ParsedString> {
    let rest = rest.split('?').next().unwrap_or_default();
    if rest.is_empty() || rest.contains(":memory:") || rest.starts_with(":resource:") {
        return Err("in-memory SQLite databases can't be compared".into());
    }
    let mut path = percent_decode(rest);
    if let Some(p) = path.strip_prefix("//") {
        // `sqlite:///abs` → `/abs`; `sqlite://C:/x` → `C:/x`.
        path = p.to_owned();
        if !path.starts_with('/') && !is_drive_path(&path) {
            path.insert(0, '/');
        }
    }
    // `/C:/x.db` is a Windows drive path.
    if path.starts_with('/') && is_drive_path(&path[1..]) {
        path.remove(0);
    }
    let mut out = ParsedString {
        config: ConnectionDraft::new(Engine::Sqlite),
        ..Default::default()
    };
    out.config.file = path;
    Ok(out)
}

fn is_drive_path(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- JDBC ----------

fn jdbc(rest: &str) -> Res<ParsedString> {
    let (sub, after) = rest.split_once(':').ok_or("incomplete JDBC URL")?;
    match sub.to_ascii_lowercase().as_str() {
        "sqlserver" => sql_server_jdbc(after.trim_start_matches('/')),
        "jtds" => {
            let after = strip_prefix_ci(after, "sqlserver:").ok_or("only jTDS SQL Server URLs are supported")?;
            jtds(after.trim_start_matches('/'))
        }
        "postgresql" | "pgsql" => {
            // `jdbc:postgresql:dbname` and `jdbc:postgresql:/` mean localhost.
            if after.starts_with("//") {
                url("postgresql", after)
            } else {
                url("postgresql", &format!("//localhost/{}", after.trim_start_matches('/')))
            }
        }
        // `jdbc:mysql:loadbalance://…`, `jdbc:mysql:replication://…`
        "mysql" | "mariadb" => {
            let after = after.split_once("//").map_or(after, |(_, r)| r);
            url("mysql", &format!("//{after}"))
        }
        "sqlite" => sqlite_path(after),
        other => Err(format!("`jdbc:{other}` databases aren't supported")),
    }
}

/// `host[\instance][:port][;prop=value]*`, any part optional.
fn sql_server_jdbc(s: &str) -> Res<ParsedString> {
    let mut out = ParsedString {
        config: ConnectionDraft::new(Engine::Mssql),
        ..Default::default()
    };
    let (server, props) = s.split_once(';').unwrap_or((s, ""));
    let mut server = server.to_owned();
    let props = key_value_pairs(props);
    let prop = |k: &str| props.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    if let Some(name) = prop("servername").or(prop("server")) {
        if server.is_empty() {
            server = name.to_owned();
        }
    }
    // JDBC writes `host\inst:port`; SQL Server's own form is `host\inst,port`.
    let (host_inst, port) = match server.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_owned(), Some(p)),
        _ => (server.clone(), None),
    };
    let mut addr = host_inst;
    if let Some(inst) = prop("instancename") {
        addr = format!("{addr}\\{inst}");
    }
    let port = port.or(prop("portnumber").or(prop("port")));
    if let Some(p) = port {
        addr = format!("{addr},{p}");
    }
    apply_sql_server(&addr, &mut out.config, &mut out.notes)?;
    let mut encrypt = Encrypt::Mandatory; // mssql-jdbc 10.2+ encrypts by default
    let mut trust = false;
    for (k, v) in &props {
        match k.as_str() {
            "databasename" | "database" => out.config.database = v.clone(),
            "user" | "username" | "userid" => out.config.user = v.clone(),
            "password" => out.password = Some(v.clone()),
            "integratedsecurity" if truthy(v) => out.config.auth = Auth::Integrated,
            "authentication" => auth_keyword(v, &mut out),
            "encrypt" => encrypt = Encrypt::parse(v).unwrap_or(encrypt),
            "trustservercertificate" => trust = truthy(v),
            _ => {}
        }
    }
    sql_server_tls(encrypt, trust, &mut out.config);
    Ok(out)
}

/// `jdbc:jtds:sqlserver://host[:port][/database][;prop=value]*`
fn jtds(s: &str) -> Res<ParsedString> {
    let mut out = ParsedString {
        config: ConnectionDraft::new(Engine::Mssql),
        ..Default::default()
    };
    let (main, props) = s.split_once(';').unwrap_or((s, ""));
    let (hostport, db) = main.split_once('/').unwrap_or((main, ""));
    let (host, port) = split_host_port(hostport);
    let props = key_value_pairs(props);
    let mut addr = host.to_owned();
    let mut domain = None;
    let mut encrypt = Encrypt::Optional;
    for (k, v) in &props {
        match k.as_str() {
            "instance" => addr = format!("{addr}\\{v}"),
            "user" => out.config.user = v.clone(),
            "password" => out.password = Some(v.clone()),
            "domain" => domain = Some(v.clone()),
            "ssl" if v.eq_ignore_ascii_case("require") || v.eq_ignore_ascii_case("authenticate") => {
                encrypt = Encrypt::Mandatory
            }
            "databasename" => out.config.database = v.clone(),
            _ => {}
        }
    }
    if let Some(p) = port {
        addr = format!("{addr},{p}");
    }
    apply_sql_server(&addr, &mut out.config, &mut out.notes)?;
    if !db.is_empty() {
        out.config.database = percent_decode(db);
    }
    if let Some(d) = domain {
        if out.config.user.is_empty() {
            out.config.auth = Auth::Integrated;
        } else {
            out.notes.push(format!("Signed in to the {d} domain with a named account; set the login up again if Windows authentication isn't what you want"));
            out.config.auth = Auth::Integrated;
            out.config.user.clear();
        }
    }
    sql_server_tls(encrypt, false, &mut out.config);
    Ok(out)
}

/// SQL Server's `Authentication=` keyword (SqlClient / JDBC spellings).
fn auth_keyword(v: &str, out: &mut ParsedString) {
    let v: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match v.as_str() {
        "sqlpassword" | "" => {}
        // `Active Directory Integrated` is Microsoft Entra ID, not Windows authentication.
        "windows" | "ntlm" => out.config.auth = Auth::Integrated,
        _ => out.notes.push(format!(
            "Signs in with Microsoft Entra ID ({v}), which PCompare doesn't support yet; enter a SQL login instead"
        )),
    }
}

// ---------- key=value lists ----------

/// Splits `key=value;…` with ADO.NET quoting (`"…"`, `'…'`, doubled quotes)
/// and ODBC braces (`{…}`, `}}`). Keys come back lower-cased with spaces and
/// underscores removed (`Initial Catalog` → `initialcatalog`).
fn key_value_pairs(s: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ';' {
            i += 1;
        }
        let key: String = chars[start..i]
            .iter()
            .filter(|c| !c.is_whitespace() && **c != '_')
            .collect::<String>()
            .to_ascii_lowercase();
        if i >= chars.len() || chars[i] == ';' {
            i += 1;
            continue;
        }
        i += 1; // '='
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        match chars.get(i) {
            Some(&q @ ('"' | '\'')) => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == q {
                        if chars.get(i + 1) == Some(&q) {
                            value.push(q);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    value.push(chars[i]);
                    i += 1;
                }
                while i < chars.len() && chars[i] != ';' {
                    i += 1;
                }
            }
            Some('{') => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '}' {
                        if chars.get(i + 1) == Some(&'}') {
                            value.push('}');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    value.push(chars[i]);
                    i += 1;
                }
                while i < chars.len() && chars[i] != ';' {
                    i += 1;
                }
            }
            _ => {
                let vs = i;
                while i < chars.len() && chars[i] != ';' {
                    i += 1;
                }
                value = chars[vs..i].iter().collect::<String>().trim().to_owned();
            }
        }
        i += 1; // ';'
        if !key.is_empty() {
            out.push((key, value));
        }
    }
    out
}

/// Which engine a `key=value` list is for, from its driver/provider or the
/// keywords only one family of drivers uses. `None` = can't tell.
fn detect_engine(pairs: &[(String, String)]) -> Res<Option<Engine>> {
    let get = |k: &str| pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.to_ascii_lowercase());
    let has = |k: &str| pairs.iter().any(|(key, _)| key == k);
    if let Some(d) = get("driver") {
        return Ok(Some(if d.contains("sql server") || d.contains("sql native client") {
            Engine::Mssql
        } else if d.contains("mysql") || d.contains("mariadb") {
            Engine::Mysql
        } else if d.contains("postgres") || d.contains("psql") {
            Engine::Postgres
        } else if d.contains("sqlite") {
            Engine::Sqlite
        } else {
            return Err(format!("the ODBC driver `{d}` isn't a database PCompare supports"));
        }));
    }
    if let Some(p) = get("provider") {
        if p.starts_with("sqloledb") || p.starts_with("msoledbsql") || p.starts_with("sqlncli") {
            return Ok(Some(Engine::Mssql));
        }
        if p.contains("postgres") || p.starts_with("pgnp") {
            return Ok(Some(Engine::Postgres));
        }
        if p.contains("mysql") {
            return Ok(Some(Engine::Mysql));
        }
        if !p.starts_with("msdasql") {
            return Err(format!("the OLE DB provider `{p}` isn't a database PCompare supports"));
        }
    }
    let port = get("port").and_then(|p| p.parse::<u16>().ok());
    let sql_server_only = [
        "initialcatalog",
        "integratedsecurity",
        "trustedconnection",
        "trustservercertificate",
        "multipleactiveresultsets",
        "applicationintent",
        "multisubnetfailover",
        "useencryptionfordata",
        "attachdbfilename",
    ];
    if sql_server_only.iter().any(|k| has(k)) || port == Some(1433) {
        return Ok(Some(Engine::Mssql));
    }
    let ssl_mode = get("sslmode").unwrap_or_default();
    if port == Some(5432) || has("searchpath") || (has("username") && !has("uid")) || matches!(ssl_mode.as_str(), "prefer" | "verify-full") {
        return Ok(Some(Engine::Postgres));
    }
    let mysql_only = ["allowuservariables", "charset", "allowpublickeyretrieval", "convertzerodatetime", "sslca"];
    if port == Some(3306) || mysql_only.iter().any(|k| has(k)) || matches!(ssl_mode.as_str(), "preferred" | "required" | "none") {
        return Ok(Some(Engine::Mysql));
    }
    // `Data Source=app.db` with nothing else is a SQLite file.
    if let Some(ds) = get("datasource") {
        let file_like = [".db", ".sqlite", ".sqlite3", ".db3", ".s3db", ".sl3"].iter().any(|x| ds.ends_with(x));
        if file_like && pairs.len() <= 3 {
            return Ok(Some(Engine::Sqlite));
        }
    }
    Ok(None)
}

fn key_values(s: &str, hint: Option<Engine>) -> Res<ParsedString> {
    let pairs = key_value_pairs(s);
    if pairs.is_empty() {
        return Err("not a connection string PCompare recognises".into());
    }
    let mut notes = Vec::new();
    let engine = match detect_engine(&pairs)?.or(hint) {
        Some(e) => e,
        None => {
            notes.push("Couldn't tell which database this is for; assumed SQL Server — change the engine if it's wrong".into());
            Engine::Mssql
        }
    };
    let mut out = ParsedString {
        config: ConnectionDraft::new(engine),
        notes,
        ..Default::default()
    };
    if engine == Engine::Sqlite {
        let file = pairs
            .iter()
            .find(|(k, _)| matches!(k.as_str(), "datasource" | "database" | "filename"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if file.is_empty() || file == ":memory:" {
            return Err("in-memory SQLite databases can't be compared".into());
        }
        out.config.file = file;
        return Ok(out);
    }
    let mut server = String::new();
    let mut encrypt = Encrypt::Optional; // older SQL Server clients' default
    let mut trust = false;
    for (k, v) in &pairs {
        match k.as_str() {
            "datasource" | "server" | "address" | "addr" | "networkaddress" | "host" | "hostname" | "servername" => {
                if server.is_empty() {
                    server = v.clone()
                }
            }
            "initialcatalog" | "database" | "dbname" => out.config.database = v.clone(),
            "userid" | "uid" | "user" | "username" => out.config.user = v.clone(),
            "password" | "pwd" => out.password = Some(v.clone()),
            "port" => out.config.port = v.parse().ok(),
            "integratedsecurity" | "trustedconnection" if truthy(v) => out.config.auth = Auth::Integrated,
            "authentication" => auth_keyword(v, &mut out),
            "encrypt" | "useencryptionfordata" => encrypt = Encrypt::parse(v).unwrap_or(encrypt),
            "trustservercertificate" => trust = truthy(v),
            "sslmode" | "ssl" => apply_ssl_mode(&mut out.config, v),
            "searchpath" => out.config.schemas = split_list(v),
            _ => {}
        }
    }
    if engine == Engine::Mssql {
        apply_sql_server(&server, &mut out.config, &mut out.notes)?;
        if out.config.port.is_none() {
            out.config.port = pairs.iter().find(|(k, _)| k == "port").and_then(|(_, v)| v.parse().ok());
        }
        sql_server_tls(encrypt, trust, &mut out.config);
        if out.config.auth == Auth::Integrated {
            out.config.user.clear();
        }
    } else {
        // Npgsql / MySQL Connector accept `host1,host2` and `host:port`.
        let first = server.split(',').next().unwrap_or_default().trim();
        if server.contains(',') {
            out.notes.push("Lists several hosts; PCompare connects to the first".into());
        }
        let (h, p) = split_host_port(first);
        out.config.host = if h.is_empty() { "localhost".into() } else { h.to_owned() };
        out.config.port = out.config.port.or(p);
        if trust && out.config.tls != Tls::Disable {
            out.config.trust_server_certificate = true;
        }
    }
    Ok(out)
}

/// PostgreSQL `sslmode` / MySQL `ssl-mode` values and the `ssl=true` booleans.
fn apply_ssl_mode(cfg: &mut ConnectionDraft, v: &str) {
    let v = v.trim().to_ascii_lowercase().replace(['_', '-'], "");
    cfg.tls = match v.as_str() {
        "disable" | "disabled" | "none" | "false" | "off" | "0" => Tls::Disable,
        "allow" | "prefer" | "preferred" => Tls::Prefer,
        "require" | "required" | "true" | "on" | "1" => Tls::Require,
        // PCompare doesn't pin a CA, so verification becomes "require".
        "verifyca" | "verifyfull" | "verifyidentity" => Tls::Require,
        _ => cfg.tls,
    };
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().trim_matches('"').to_owned()).filter(|s| !s.is_empty() && s != "$user").collect()
}

/// URL query parameters and JDBC properties for PostgreSQL / MySQL / SQL Server URLs.
fn apply_params(out: &mut ParsedString, params: impl Iterator<Item = (String, String)>) {
    let mut encrypt = None;
    let mut trust = false;
    for (k, v) in params {
        let k = k.to_ascii_lowercase().replace(['_', '-'], "");
        match k.as_str() {
            "user" | "username" => out.config.user = v,
            "password" => out.password = Some(v),
            "sslmode" | "ssl" | "usessl" | "requiressl" => apply_ssl_mode(&mut out.config, &v),
            "currentschema" | "searchpath" => out.config.schemas = split_list(&v),
            "encrypt" => encrypt = Encrypt::parse(&v),
            "trustservercertificate" => trust = truthy(&v),
            "database" | "databasename" => out.config.database = v,
            _ => {}
        }
    }
    if out.config.engine == Engine::Mssql {
        sql_server_tls(encrypt.unwrap_or(Encrypt::Optional), trust, &mut out.config);
    }
}

// ---------- libpq ----------

/// `host=… port=…` without semicolons and with a libpq-only keyword.
fn looks_like_libpq(s: &str) -> bool {
    !s.contains(';') && s.split_whitespace().count() > 1 && s.split_whitespace().any(|w| w.starts_with("dbname=") || w.starts_with("host=") || w.starts_with("hostaddr="))
}

/// libpq `keyword=value` pairs separated by spaces; values may be
/// single-quoted with `\'` / `\\` escapes.
fn libpq(s: &str) -> Res<ParsedString> {
    let mut out = ParsedString {
        config: ConnectionDraft::new(Engine::Postgres),
        ..Default::default()
    };
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        let key: String = chars[ks..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if chars.get(i) != Some(&'=') {
            continue;
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if chars.get(i) == Some(&'\'') {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                value.push(chars[i]);
                i += 1;
            }
            i += 1;
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                value.push(chars[i]);
                i += 1;
            }
        }
        match key.as_str() {
            "host" | "hostaddr" => {
                if out.config.host.is_empty() {
                    out.config.host = value.split(',').next().unwrap_or_default().to_owned()
                }
            }
            "port" => out.config.port = value.split(',').next().and_then(|p| p.parse().ok()),
            "dbname" => out.config.database = value,
            "user" => out.config.user = value,
            "password" => out.password = Some(value),
            "sslmode" => apply_ssl_mode(&mut out.config, &value),
            _ => {}
        }
    }
    if out.config.host.is_empty() || out.config.host.starts_with('/') {
        // A Unix-socket directory; PCompare connects over TCP.
        out.config.host = "localhost".into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ParsedString {
        parse(s, None).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn ado_net_sql_server_strings() {
        let r = p(r#"Data Source=tcp:db01\SQLEXPRESS;Initial Catalog=Shop;User ID=app;Password="a;b""c";Encrypt=True;TrustServerCertificate=False"#);
        let c = &r.config;
        assert_eq!(c.engine, Engine::Mssql);
        assert_eq!((c.host.as_str(), c.instance.as_str(), c.database.as_str(), c.user.as_str()), ("db01", "SQLEXPRESS", "Shop", "app"));
        assert_eq!(r.password.as_deref(), Some("a;b\"c"));
        assert_eq!((c.tls, c.trust_server_certificate), (Tls::Require, false));

        let r = p("Server=.;Database=master;Integrated Security=SSPI");
        assert_eq!((r.config.host.as_str(), r.config.auth), ("localhost", Auth::Integrated));
        assert_eq!((r.config.tls, r.config.trust_server_certificate), (Tls::Prefer, true));
    }

    #[test]
    fn odbc_and_ole_db_strings() {
        let r = p("Driver={ODBC Driver 18 for SQL Server};Server=sql.corp,1500;Database=hr;Trusted_Connection=yes;Encrypt=strict");
        assert_eq!(r.config.engine, Engine::Mssql);
        assert_eq!((r.config.port, r.config.auth, r.config.tls), (Some(1500), Auth::Integrated, Tls::Require));

        let r = p("Driver={PostgreSQL Unicode};Server=pg;Port=5433;Database=app;Uid=me;Pwd=x;sslmode=require");
        assert_eq!((r.config.engine, r.config.port, r.config.tls), (Engine::Postgres, Some(5433), Tls::Require));

        let r = p("Provider=MSOLEDBSQL.1;Persist Security Info=False;User ID=sa;Initial Catalog=Ops;Data Source=sql9;Use Encryption for Data=Mandatory");
        assert_eq!((r.config.host.as_str(), r.config.database.as_str(), r.config.tls), ("sql9", "Ops", Tls::Require));

        assert!(parse("Provider=Microsoft.ACE.OLEDB.12.0;Data Source=x.accdb", None).is_err());
    }

    #[test]
    fn npgsql_and_mysql_connector_strings() {
        let r = p("Host=pg.local;Port=5432;Database=app;Username=me;SSL Mode=Prefer;Search Path=sales,public");
        assert_eq!(r.config.engine, Engine::Postgres);
        assert_eq!(r.config.schemas, vec!["sales", "public"]);
        let r = p("Server=my.local;Port=3306;Database=shop;Uid=root;SslMode=Required");
        assert_eq!((r.config.engine, r.config.tls), (Engine::Mysql, Tls::Require));
        // Ambiguous: assumed SQL Server unless the caller knows better.
        let r = p("Server=x;Database=y");
        assert_eq!((r.config.engine, r.notes.len()), (Engine::Mssql, 1));
        let r = parse("Server=x;Database=y", Some(Engine::Mysql)).unwrap();
        assert!(r.notes.is_empty() && r.config.engine == Engine::Mysql);
        assert_eq!(p("Data Source=C:\\data\\app.db").config.engine, Engine::Sqlite);
    }

    #[test]
    fn urls() {
        let r = p("postgresql://me:p%40ss@[::1]:6543/app%20db?sslmode=verify-full&currentSchema=billing");
        let c = &r.config;
        assert_eq!((c.host.as_str(), c.port, c.database.as_str(), c.user.as_str()), ("::1", Some(6543), "app db", "me"));
        assert_eq!((r.password.as_deref(), c.tls), (Some("p@ss"), Tls::Require));
        assert_eq!(c.schemas, vec!["billing"]);
        let r = p("mysql://root@db:3307/shop?ssl-mode=DISABLED");
        assert_eq!((r.config.engine, r.config.port, r.config.tls), (Engine::Mysql, Some(3307), Tls::Disable));
        let r = p("mssql://sa@sql01%5CPROD/ops?encrypt=true");
        assert_eq!((r.config.host.as_str(), r.config.instance.as_str(), r.config.tls), ("sql01", "PROD", Tls::Require));
        assert_eq!(p("sqlite:///var/data/app.db").config.file, "/var/data/app.db");
        assert_eq!(p("sqlite:///C:/data/app.db").config.file, "C:/data/app.db");
        assert_eq!(p("\"postgres://h/d\"").config.database, "d");
        assert!(parse("sqlite::memory:", None).is_err());
    }

    #[test]
    fn jdbc_urls() {
        let r = p(r"jdbc:sqlserver://sql01\PROD;databaseName=Ops;integratedSecurity=true;encrypt=false");
        let c = &r.config;
        assert_eq!((c.host.as_str(), c.instance.as_str(), c.database.as_str()), ("sql01", "PROD", "Ops"));
        assert_eq!((c.auth, c.tls, c.trust_server_certificate), (Auth::Integrated, Tls::Prefer, true));
        let r = p("jdbc:sqlserver://sql01:1500;database=Ops;user=sa;trustServerCertificate=true");
        assert_eq!((r.config.port, r.config.tls, r.config.trust_server_certificate), (Some(1500), Tls::Require, true));
        let r = p("jdbc:jtds:sqlserver://old:1433/legacy;instance=SQL2008");
        assert_eq!((r.config.database.as_str(), r.config.port), ("legacy", Some(1433)));
        let r = p("jdbc:postgresql://pg1,pg2:5433/app?user=me&ssl=true");
        assert_eq!((r.config.host.as_str(), r.config.user.as_str(), r.config.tls), ("pg1", "me", Tls::Require));
        assert_eq!(r.notes.len(), 1);
        assert_eq!(p("jdbc:postgresql:app").config.host, "localhost");
        let r = p("jdbc:mariadb://maria:3306/shop?useSSL=false");
        assert_eq!((r.config.engine, r.config.tls), (Engine::Mysql, Tls::Disable));
        assert_eq!(p("jdbc:mysql:replication://a,b/shop").config.host, "a");
        assert_eq!(p(r"jdbc:sqlite:C:\data\app.db").config.file, r"C:\data\app.db");
        assert!(parse("jdbc:oracle:thin:@h:1521:x", None).is_err());
    }

    #[test]
    fn libpq_keyword_strings() {
        let r = p("host=pg.local port=5433 dbname='my app' user=me sslmode=disable");
        let c = &r.config;
        assert_eq!((c.engine, c.host.as_str(), c.port, c.database.as_str(), c.tls), (Engine::Postgres, "pg.local", Some(5433), "my app", Tls::Disable));
        assert_eq!(p("host=/var/run/postgresql dbname=x").config.host, "localhost");
    }
}
