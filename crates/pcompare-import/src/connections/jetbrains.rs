//! JetBrains IDEs' data sources (DataGrip, IntelliJ IDEA, Rider…):
//! `dataSources.xml` in a project's `.idea` folder or the IDE's `options`
//! folder (global data sources), plus the per-user `dataSources.local.xml`
//! next to it, matched by `uuid`.
//!
//! ```xml
//! <data-source source="LOCAL" name="sales@sql01" group="Prod" uuid="…">
//!   <driver-ref>sqlserver.ms</driver-ref>
//!   <jdbc-url>jdbc:sqlserver://localhost;instanceName=SQLEXPRESS;databaseName=Sales</jdbc-url>
//! </data-source>
//! <!-- dataSources.local.xml -->
//! <data-source name="sales@sql01" uuid="…">
//!   <user-name>app</user-name> <auth-provider>ms-sso</auth-provider>
//!   <ssl-config><enabled>true</enabled><mode>REQUIRE</mode></ssl-config>
//! </data-source>
//! ```
//!
//! Everything about the server is in the JDBC URL (`sqlserver.jb` uses an
//! ADO.NET string instead). Passwords live in the IDE's keychain; one in
//! the URL is dropped.

use super::conn_string;
use super::{Auth, Engine, ImportedConnection, Parsed, Tls};
use crate::{Result, Skipped};
use roxmltree::Node;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn is_data_sources(root: Node) -> bool {
    matches!(root.tag_name().name(), "project" | "application")
        && root.descendants().any(|n| n.has_tag_name("data-source"))
}

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> &'a str {
    node.children()
        .find(|c| c.has_tag_name(name))
        .and_then(|c| c.text())
        .map(str::trim)
        .unwrap_or_default()
}

/// Trims whitespace and the zero-width characters seen in real `group` values.
fn clean(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{200B}' || c == '\u{FEFF}')
}

/// Per-user details from `dataSources.local.xml`, by data-source uuid.
#[derive(Default)]
struct Local {
    user: String,
    auth_provider: String,
    ssl: Option<String>,
}

fn read_local(path: &Path) -> HashMap<String, Local> {
    let Ok(text) = std::fs::read(path) else {
        return HashMap::new();
    };
    let text = crate::decode(&text);
    let Ok(doc) = roxmltree::Document::parse(&text) else {
        return HashMap::new();
    };
    doc.descendants()
        .filter(|n| n.has_tag_name("data-source"))
        .filter_map(|ds| {
            let uuid = ds.attribute("uuid")?.to_owned();
            let ssl = ds.children().find(|c| c.has_tag_name("ssl-config")).and_then(|s| {
                (child_text(s, "enabled") == "true").then(|| child_text(s, "mode").to_owned())
            });
            Some((
                uuid,
                Local {
                    user: child_text(ds, "user-name").to_owned(),
                    auth_provider: child_text(ds, "auth-provider").to_owned(),
                    ssl,
                },
            ))
        })
        .collect()
}

fn engine(driver_ref: &str) -> Option<Engine> {
    let d = driver_ref.to_ascii_lowercase();
    Some(if d.starts_with("sqlserver") || d.starts_with("azure") {
        Engine::Mssql
    } else if d.starts_with("mysql") || d.starts_with("mariadb") {
        Engine::Mysql
    } else if d.starts_with("postgres") {
        Engine::Postgres
    } else if d.starts_with("sqlite") {
        Engine::Sqlite
    } else {
        return None;
    })
}

/// `$USER_HOME$` and `$PROJECT_DIR$`-style macros in SQLite paths.
fn expand(path: &str, home: Option<&Path>, project: Option<&Path>) -> String {
    let mut out = path.to_owned();
    if let Some(h) = home {
        out = out.replace("$USER_HOME$", &h.display().to_string());
    }
    if let Some(p) = project {
        for m in ["$PROJECT_DIR$", "$ProjectFileDir$"] {
            out = out.replace(m, &p.display().to_string());
        }
    }
    out
}

/// "DataGrip 2025.2", "IntelliJ IDEA 2025.2 (global)" or "DataGrip project shop".
fn label(path: &Path) -> String {
    let dir = path.parent();
    let parent_name = |p: Option<&Path>| p.and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_owned();
    match dir.and_then(|d| d.file_name()).and_then(|n| n.to_str()) {
        Some("options") => format!("{} (global data sources)", product_label(&parent_name(dir.and_then(Path::parent)))),
        Some(".idea") => {
            let project = parent_name(dir.and_then(Path::parent));
            if project == "default" {
                // `<config>/projects/default/.idea`, before DataGrip 2021.2.
                let config = dir.and_then(Path::parent).and_then(Path::parent).and_then(Path::parent);
                product_label(&parent_name(config))
            } else {
                format!("JetBrains project {project}")
            }
        }
        _ => "JetBrains data sources".into(),
    }
}

/// `DataGrip2025.2` → `DataGrip 2025.2`, `IntelliJIdea2025.2` → `IntelliJ IDEA 2025.2`.
fn product_label(dir: &str) -> String {
    let split = dir.find(|c: char| c.is_ascii_digit()).unwrap_or(dir.len());
    let (product, version) = dir.split_at(split);
    let product = match product {
        "IntelliJIdea" | "IdeaIC" => "IntelliJ IDEA",
        "" => "JetBrains",
        p => p,
    };
    format!("{product} {version}").trim().to_owned()
}

pub(crate) fn parse(root: Node, path: &Path) -> Result<Parsed> {
    let mut out = Parsed {
        label: label(path),
        ..Default::default()
    };
    let local = read_local(&path.with_file_name("dataSources.local.xml"));
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let project = path.parent().filter(|d| d.ends_with(".idea")).and_then(Path::parent);
    for ds in root.descendants().filter(|n| n.has_tag_name("data-source") && n.attribute("name").is_some()) {
        let name = clean(ds.attribute("name").unwrap_or_default());
        let driver = child_text(ds, "driver-ref");
        let url = child_text(ds, "jdbc-url");
        let Some(engine) = engine(driver) else {
            out.skipped.push(Skipped {
                name: name.to_owned(),
                reason: format!("`{driver}` isn't a database PCompare supports"),
            });
            continue;
        };
        let url = if engine == Engine::Sqlite { expand(url, home.as_deref(), project) } else { url.to_owned() };
        // The password, if the URL has one, is dropped.
        let parsed = match conn_string::parse(&url, Some(engine)) {
            Ok(p) => p,
            Err(reason) => {
                out.skipped.push(Skipped {
                    name: name.to_owned(),
                    reason,
                });
                continue;
            }
        };
        let (mut cfg, mut notes) = (parsed.config, parsed.notes);
        let mine = ds.attribute("uuid").and_then(|u| local.get(u));
        if let Some(l) = mine {
            if l.auth_provider == "ms-sso" && engine == Engine::Mssql {
                cfg.auth = Auth::Integrated;
            }
            if cfg.auth == Auth::Password && !l.user.is_empty() {
                cfg.user = l.user.clone();
            }
            if let Some(mode) = &l.ssl {
                if engine != Engine::Mssql {
                    cfg.tls = if mode.eq_ignore_ascii_case("PREFER") { Tls::Prefer } else { Tls::Require };
                }
            }
        }
        if cfg.auth == Auth::Integrated {
            cfg.user.clear();
        }
        if mine.is_none() && cfg.user.is_empty() && cfg.auth == Auth::Password && engine != Engine::Sqlite {
            notes.push("The login name is kept per user (dataSources.local.xml), which wasn't found next to this file".into());
        }
        let group = Some(clean(ds.attribute("group").unwrap_or_default()).to_owned()).filter(|g| !g.is_empty());
        out.connections.push(ImportedConnection {
            name: if name.is_empty() { cfg.label() } else { name.to_owned() },
            group,
            config: cfg,
            notes,
        });
    }
    Ok(out)
}

/// Global data sources of the newest version of each JetBrains IDE, the
/// pre-2021.2 DataGrip default project, and `~/DataGripProjects/*`.
pub(crate) fn locations(appdata: Option<PathBuf>, home: Option<PathBuf>, xdg_config: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(a) = appdata {
        roots.push(a.join("JetBrains"));
    }
    if let Some(h) = &home {
        roots.push(h.join("Library").join("Application Support").join("JetBrains"));
    }
    if let Some(x) = xdg_config.or_else(|| home.as_ref().map(|h| h.join(".config"))) {
        roots.push(x.join("JetBrains"));
    }
    let mut out = Vec::new();
    for root in roots {
        // Newest version per product (settings are copied forward on upgrade).
        let mut newest: HashMap<String, (Vec<u32>, PathBuf)> = HashMap::new();
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let split = name.find(|c: char| c.is_ascii_digit()).unwrap_or(name.len());
            let (product, version) = name.split_at(split);
            let v: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
            if product.is_empty() || v.is_empty() {
                continue;
            }
            if newest.get(product).is_none_or(|(have, _)| *have < v) {
                newest.insert(product.to_owned(), (v, e.path()));
            }
        }
        for (_, (_, dir)) in newest {
            let label = product_label(&dir.file_name().unwrap_or_default().to_string_lossy());
            out.push((format!("{label} (global data sources)"), dir.join("options").join("dataSources.xml")));
            out.push((label, dir.join("projects").join("default").join(".idea").join("dataSources.xml")));
        }
    }
    if let Some(h) = home {
        for e in std::fs::read_dir(h.join("DataGripProjects")).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            out.push((format!("DataGrip project {name}"), e.path().join(".idea").join("dataSources.xml")));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_urls_and_the_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let idea = dir.path().join("shop").join(".idea");
        std::fs::create_dir_all(&idea).unwrap();
        std::fs::write(
            idea.join("dataSources.local.xml"),
            r#"<project version="4"><component name="dataSourceStorageLocal">
                 <data-source name="sales" uuid="u1"><auth-provider>ms-sso</auth-provider></data-source>
                 <data-source name="pg" uuid="u2"><user-name>me</user-name>
                   <ssl-config><enabled>true</enabled><mode>VERIFY_FULL</mode></ssl-config></data-source>
               </component></project>"#,
        )
        .unwrap();
        let main = r#"<project version="4"><component name="DataSourceManagerImpl" format="xml" multifile-model="true">
          <data-source source="LOCAL" name="sales" group="&#x200B;Prod" uuid="u1">
            <driver-ref>sqlserver.ms</driver-ref>
            <jdbc-url>jdbc:sqlserver://localhost;instanceName=SQLEXPRESS;databaseName=Sales;encrypt=true;trustServerCertificate=true</jdbc-url>
          </data-source>
          <data-source source="LOCAL" name="pg" uuid="u2">
            <driver-ref>postgresql</driver-ref>
            <jdbc-url>jdbc:postgresql://pg.local:5432/app</jdbc-url>
          </data-source>
          <data-source source="LOCAL" name="legacy" uuid="u3">
            <driver-ref>sqlserver.jb</driver-ref>
            <jdbc-url>Server=old;Database=Ops;User Id=ops;Password=secret</jdbc-url>
          </data-source>
          <data-source source="LOCAL" name="local db" uuid="u4">
            <driver-ref>sqlite.xerial</driver-ref>
            <jdbc-url>jdbc:sqlite:$PROJECT_DIR$/data/app.db</jdbc-url>
          </data-source>
          <data-source source="LOCAL" name="ora" uuid="u5"><driver-ref>oracle</driver-ref></data-source>
        </component></project>"#;
        let path = idea.join("dataSources.xml");
        let doc = roxmltree::Document::parse(main).unwrap();
        assert!(is_data_sources(doc.root_element()));
        let p = parse(doc.root_element(), &path).unwrap();
        assert_eq!(p.label, "JetBrains project shop");
        assert_eq!(p.connections.len(), 4);
        let sales = &p.connections[0];
        assert_eq!(sales.group.as_deref(), Some("Prod"), "zero-width space trimmed");
        let c = &sales.config;
        assert_eq!((c.instance.as_str(), c.database.as_str(), c.auth, c.tls), ("SQLEXPRESS", "Sales", Auth::Integrated, Tls::Require));
        let pg = &p.connections[1].config;
        assert_eq!((pg.user.as_str(), pg.tls), ("me", Tls::Require));
        let legacy = &p.connections[2].config;
        assert_eq!((legacy.engine, legacy.host.as_str(), legacy.user.as_str()), (Engine::Mssql, "old", "ops"));
        let file = &p.connections[3].config.file;
        assert!(file.ends_with("app.db") && file.contains("shop"), "{file}");
        assert_eq!(p.skipped.len(), 1);
        assert!(!format!("{p:?}").contains("secret"));
    }

    #[test]
    fn product_folders_read_nicely() {
        assert_eq!(product_label("DataGrip2025.2"), "DataGrip 2025.2");
        assert_eq!(product_label("IntelliJIdea2024.3"), "IntelliJ IDEA 2024.3");
        let p = PathBuf::from("JetBrains").join("Rider2025.1").join("options").join("dataSources.xml");
        assert_eq!(label(&p), "Rider 2025.1 (global data sources)");
    }
}
