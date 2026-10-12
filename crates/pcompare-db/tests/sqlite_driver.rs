//! SQLite driver + dialect against real database files.

use pcompare_db::connection::ConnectionConfig;
use pcompare_db::dialect::dialect;
use pcompare_db::driver::Client;
use pcompare_db::model::*;
use pcompare_db::value::Value;
use std::path::Path;

const FIXTURE: &[&str] = &[
    "CREATE TABLE users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        email TEXT NOT NULL COLLATE NOCASE UNIQUE,
        name VARCHAR(50) DEFAULT 'anon',
        score REAL CHECK (score >= 0),
        CONSTRAINT email_shape CHECK (email LIKE '%@%')
    )",
    "CREATE TABLE order_lines (
        order_id INTEGER NOT NULL,
        line_no INTEGER NOT NULL,
        user_id INTEGER REFERENCES users ON DELETE CASCADE,
        sku TEXT,
        qty INT NOT NULL DEFAULT 1,
        price NUMERIC(10,2),
        total NUMERIC GENERATED ALWAYS AS (qty * price) VIRTUAL,
        PRIMARY KEY (order_id, line_no),
        UNIQUE (order_id, sku),
        CONSTRAINT fk_named FOREIGN KEY (user_id, sku) REFERENCES products (owner_id, sku) ON UPDATE SET NULL
    )",
    "CREATE TABLE products (owner_id INTEGER, sku TEXT, PRIMARY KEY (owner_id, sku)) WITHOUT ROWID",
    "CREATE INDEX ix_lines_sku ON order_lines (sku DESC) WHERE qty > 0",
    "CREATE INDEX ix_users_lower ON users (lower(name), \"id\")",
    "CREATE VIEW big_spenders AS SELECT id, email FROM users WHERE score > 100",
    "CREATE TRIGGER trg_users_name AFTER INSERT ON users BEGIN UPDATE users SET name = upper(name) WHERE id = NEW.id; END",
];

fn new_db(dir: &Path, name: &str) -> ConnectionConfig {
    let file = dir.join(name);
    std::fs::File::create(&file).unwrap(); // an empty file is an empty database
    ConnectionConfig { engine: Some(Engine::Sqlite), file: file.to_string_lossy().into_owned(), ..Default::default() }
}

async fn client_with(cfg: &ConnectionConfig, stmts: &[&str]) -> Client {
    let mut c = Client::connect(cfg).await.unwrap();
    let stmts: Vec<String> = stmts.iter().map(|s| s.to_string()).collect();
    c.execute(&stmts, true).await.unwrap();
    c
}

#[tokio::test]
async fn missing_file_is_an_error_and_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("nope.db");
    let cfg = ConnectionConfig { engine: Some(Engine::Sqlite), file: file.to_string_lossy().into_owned(), ..Default::default() };
    let e = Client::connect(&cfg).await.err().expect("must fail").to_string();
    assert!(e.contains("does not exist"), "{e}");
    assert!(!file.exists());
}

#[tokio::test]
async fn introspects_schema() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = new_db(dir.path(), "shop.db");
    let mut c = client_with(&cfg, FIXTURE).await;
    assert!(c.server_version().await.unwrap().starts_with("SQLite 3."));
    assert!(c.list_databases().await.unwrap().is_empty());
    let s = c.load_schema().await.unwrap();
    assert_eq!(s.engine, Some(Engine::Sqlite));
    assert_eq!(s.database, "shop");
    let names: Vec<&str> = s.tables.iter().map(|t| t.name.name.as_str()).collect();
    assert_eq!(names, ["order_lines", "products", "users"]); // no sqlite_sequence
    assert!(s.tables.iter().all(|t| t.name.schema.is_empty()));

    let users = &s.tables[2];
    let id = users.column("id").unwrap();
    assert_eq!(id.data_type, "integer");
    assert_eq!(id.identity, Some(Identity::default()));
    assert!(!id.nullable);
    let email = users.column("email").unwrap();
    assert_eq!((email.nullable, email.collation.as_deref()), (false, Some("NOCASE")));
    assert_eq!(users.column("name").unwrap().default.as_deref(), Some("'anon'"));
    assert_eq!(users.column("name").unwrap().data_type, "varchar(50)");
    assert_eq!(users.primary_key.as_ref().unwrap().columns, ["id"]);
    let checks: Vec<(&str, &str)> = users.checks.iter().map(|c| (c.name.as_str(), c.expression.as_str())).collect();
    assert_eq!(checks, [("users_check1", "score >= 0"), ("email_shape", "email LIKE '%@%'")]);
    assert_eq!(users.indexes.len(), 2);
    let ix = &users.indexes[0];
    assert_eq!(ix.name, "ix_users_lower");
    assert_eq!(ix.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["lower(name)", "id"]);
    let uq = &users.indexes[1];
    assert_eq!((uq.name.as_str(), uq.unique, uq.constraint), ("users_email_key", true, true));

    let lines = &s.tables[0];
    assert_eq!(lines.primary_key.as_ref().unwrap().columns, ["order_id", "line_no"]);
    assert!(lines.column("order_id").unwrap().identity.is_none());
    let total = lines.column("total").unwrap();
    assert_eq!(total.computed.as_deref(), Some("qty * price"));
    assert_eq!(total.default, None);
    assert_eq!(lines.column("price").unwrap().data_type, "numeric(10,2)");
    let ix = lines.indexes.iter().find(|i| i.name == "ix_lines_sku").unwrap();
    assert_eq!(ix.filter.as_deref(), Some("qty > 0"));
    assert!(ix.columns[0].descending && !ix.unique && !ix.constraint);
    assert!(lines.indexes.iter().any(|i| i.name == "order_lines_order_id_sku_key" && i.constraint));
    // Synthesised names, ordered by referenced table: products < users.
    assert_eq!(lines.foreign_keys.len(), 2);
    let fk1 = &lines.foreign_keys[0];
    assert_eq!(fk1.name, "fk_order_lines_1");
    assert_eq!((fk1.ref_table.name.as_str(), fk1.columns.clone()), ("products", vec!["user_id".to_owned(), "sku".to_owned()]));
    assert_eq!((fk1.on_delete.as_deref(), fk1.on_update.as_deref()), (None, Some("SET NULL")));
    let fk2 = &lines.foreign_keys[1];
    assert_eq!(fk2.name, "fk_order_lines_2");
    assert_eq!(fk2.ref_columns, ["id"]); // implicit reference to the PK
    assert_eq!(fk2.on_delete.as_deref(), Some("CASCADE"));

    let objs: Vec<(ObjectKind, &str)> = s.objects.iter().map(|o| (o.kind, o.name.name.as_str())).collect();
    assert_eq!(objs, [(ObjectKind::View, "big_spenders"), (ObjectKind::Trigger, "trg_users_name")]);
    assert!(s.objects[0].definition.starts_with("CREATE VIEW big_spenders AS SELECT"));
    assert_eq!(s.objects[1].table, Some(ObjectName::new("", "users")));
}

/// Tables rendered by the dialect and re-read by the driver are unchanged.
#[tokio::test]
async fn dialect_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = new_db(dir.path(), "a.db");
    let source = client_with(&src, FIXTURE).await.load_schema().await.unwrap();

    let d = dialect(Engine::Sqlite);
    let mut stmts = Vec::new();
    for t in &source.tables {
        stmts.push(d.create_table(t));
        stmts.extend(t.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
    }
    stmts.extend(source.objects.iter().flat_map(|o| d.create_object(o)));
    let dst = new_db(dir.path(), "b.db");
    let mut c = Client::connect(&dst).await.unwrap();
    c.execute(&stmts, true).await.unwrap();
    let copy = c.load_schema().await.unwrap();
    assert_eq!(copy.tables, source.tables);
    assert_eq!(copy.objects, source.objects);
}

#[tokio::test]
async fn fetch_rows_canonical_values() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = new_db(dir.path(), "data.db");
    let mut c = client_with(
        &cfg,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, flag BOOLEAN, n REAL, d DATE, ts DATETIME, tz TIMESTAMP,
             u UUID, j JSON, b BLOB, s TEXT, any_col)",
            "INSERT INTO t VALUES (2, 1, 1.5, '2024-01-02', '2024-01-02 10:00:00.500', '2024-01-02T10:00:00Z',
             'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', '{\"a\": 1}', X'00ff', 'it''s', 42)",
            "INSERT INTO t VALUES (1, NULL, NULL, 'not a date', NULL, NULL, NULL, NULL, NULL, NULL, 'x')",
        ],
    )
    .await;
    let cols: Vec<String> =
        ["id", "flag", "n", "d", "ts", "tz", "u", "j", "b", "s", "any_col"].iter().map(|s| s.to_string()).collect();
    let rows = c.fetch_rows(&ObjectName::new("", "t"), &cols, &cols[..1]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Null);
    assert_eq!(rows[0][3], Value::Text("not a date".into()));
    assert_eq!(rows[0][10], Value::Text("x".into()));
    assert_eq!(
        rows[1],
        [
            Value::Int(2),
            Value::Bool(true),
            Value::Float(1.5),
            Value::Date("2024-01-02".into()),
            Value::DateTime("2024-01-02 10:00:00.5".into()),
            Value::DateTimeTz("2024-01-02 10:00:00+00:00".into()),
            Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
            Value::Document("{\"a\": 1}".into()),
            Value::Bytes(vec![0, 255]),
            Value::Text("it's".into()),
            Value::Int(42),
        ]
    );

    // Literals rendered by the dialect read back as the same values.
    let d = dialect(Engine::Sqlite);
    let mut copy = rows[1].clone();
    copy[0] = Value::Int(3);
    let insert = d.insert(&ObjectName::new("", "t"), &cols, &copy);
    c.execute(&[insert], true).await.unwrap();
    let back = c.fetch_rows(&ObjectName::new("", "t"), &cols, &cols[..1]).await.unwrap();
    assert_eq!(back[2], copy);
}

#[tokio::test]
async fn execute_rolls_back_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = new_db(dir.path(), "tx.db");
    let mut c = client_with(&cfg, &["CREATE TABLE t (id INTEGER PRIMARY KEY)"]).await;
    let stmts = vec![
        "INSERT INTO t VALUES (1)".to_owned(),
        "CREATE TABLE t2 (x)".to_owned(),
        "INSERT INTO nope VALUES (1)".to_owned(),
    ];
    let e = c.execute(&stmts, true).await.unwrap_err().to_string();
    assert!(e.contains("statement 3 failed") && e.contains("nope"), "{e}");
    let s = c.load_schema().await.unwrap();
    assert_eq!(s.tables.len(), 1);
    let rows = c.fetch_rows(&ObjectName::new("", "t"), &["id".into()], &[]).await.unwrap();
    assert!(rows.is_empty());

    // Non-transactional: statements before the failure stay applied.
    assert!(c.execute(&stmts, false).await.is_err());
    let rows = c.fetch_rows(&ObjectName::new("", "t"), &["id".into()], &[]).await.unwrap();
    assert_eq!(rows, [[Value::Int(1)]]);
}

#[tokio::test]
async fn deferred_foreign_keys_allow_any_row_order() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = new_db(dir.path(), "fk.db");
    let mut c = client_with(
        &cfg,
        &["CREATE TABLE p (id INTEGER PRIMARY KEY)", "CREATE TABLE ch (id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES p (id))"],
    )
    .await;
    let d = dialect(Engine::Sqlite);
    let child_first = vec!["INSERT INTO ch VALUES (1, 7)".to_owned(), "INSERT INTO p VALUES (7)".to_owned()];
    assert!(c.execute(&child_first, true).await.is_err(), "FKs are enforced");
    let mut stmts = d.disable_foreign_keys(&[]);
    stmts.extend(child_first);
    stmts.extend(d.enable_foreign_keys(&[]));
    c.execute(&stmts, true).await.unwrap();
}
