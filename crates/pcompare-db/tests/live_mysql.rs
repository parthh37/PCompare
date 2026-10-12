//! Live MySQL / MariaDB tests. Skipped unless `PCOMPARE_LIVE_MYSQL` holds a
//! JSON `ConnectionConfig` for a server where the login may create and drop
//! databases, e.g.
//! `{"engine":"mysql","host":"127.0.0.1","port":33306,"user":"root","database":""}`.
//! Each test works in its own scratch databases (`pcdb_<test>_*`).

use pcompare_db::connection::{ConnectionConfig, TlsMode};
use pcompare_db::dialect::dialect;
use pcompare_db::driver::Client;
use pcompare_db::model::*;
use pcompare_db::value::Value;

fn config() -> Option<ConnectionConfig> {
    let raw = std::env::var("PCOMPARE_LIVE_MYSQL").ok()?;
    Some(serde_json::from_str(&raw).expect("PCOMPARE_LIVE_MYSQL must be a ConnectionConfig JSON"))
}

fn with_db(cfg: &ConnectionConfig, db: &str) -> ConnectionConfig {
    ConnectionConfig { database: db.into(), ..cfg.clone() }
}

async fn exec(cfg: &ConnectionConfig, statements: &[&str]) {
    let stmts: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
    Client::connect(cfg).await.expect("connect").execute(&stmts, false).await.expect("execute");
}

/// Drops and recreates `dbs`.
async fn fresh(cfg: &ConnectionConfig, dbs: &[&str]) {
    let mut stmts = Vec::new();
    for db in dbs {
        stmts.push(format!("DROP DATABASE IF EXISTS `{db}`"));
        stmts.push(format!("CREATE DATABASE `{db}` CHARACTER SET utf8mb4"));
    }
    Client::connect(cfg).await.expect("connect").execute(&stmts, false).await.expect("create databases");
}

async fn drop_dbs(cfg: &ConnectionConfig, dbs: &[&str]) {
    let stmts: Vec<String> = dbs.iter().map(|db| format!("DROP DATABASE IF EXISTS `{db}`")).collect();
    let _ = Client::connect(cfg).await.expect("connect").execute(&stmts, false).await;
}

async fn load(cfg: &ConnectionConfig) -> Schema {
    Client::connect(cfg).await.expect("connect").load_schema().await.expect("load schema")
}

const FIXTURE: &[&str] = &[
    r"CREATE TABLE customers (
        id int unsigned NOT NULL AUTO_INCREMENT,
        email varchar(120) NOT NULL,
        name varchar(80) COLLATE utf8mb4_bin DEFAULT 'it''s \\ x',
        status enum('Active','blocked') NOT NULL DEFAULT 'Active',
        flag tinyint(1) NOT NULL DEFAULT 0,
        score decimal(10,2) DEFAULT 1.50,
        bio text,
        created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at timestamp(3) NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),
        touched datetime NULL ON UPDATE CURRENT_TIMESTAMP,
        joined date DEFAULT (curdate()),
        name_len int GENERATED ALWAYS AS (char_length(name)) VIRTUAL,
        email_lc varchar(120) GENERATED ALWAYS AS (lower(email)) STORED,
        PRIMARY KEY (id),
        UNIQUE KEY uq_email (email),
        KEY ix_name_prefix (name(10), status DESC),
        KEY ix_lower_name ((lower(name))),
        FULLTEXT KEY ft_bio (bio),
        CONSTRAINT ck_score CHECK (score >= 0)
    )",
    "CREATE TABLE order_items (
        order_id int NOT NULL,
        line int NOT NULL,
        customer_id int unsigned,
        qty int NOT NULL DEFAULT 1,
        PRIMARY KEY (order_id, line),
        KEY ix_customer (customer_id),
        CONSTRAINT fk_items_customer FOREIGN KEY (customer_id) REFERENCES customers (id) ON DELETE CASCADE
    )",
    "CREATE VIEW v_active AS SELECT id, email FROM customers WHERE status = 'Active'",
    "CREATE PROCEDURE p_count(OUT n int)\nBEGIN\n  SELECT count(*) INTO n FROM customers;\nEND",
    "CREATE FUNCTION f_double(x int) RETURNS int DETERMINISTIC\nBEGIN\n  RETURN x * 2;\nEND",
    "CREATE TRIGGER trg_customers_bi BEFORE INSERT ON customers FOR EACH ROW\nBEGIN\n  SET NEW.email = lower(NEW.email);\nEND",
];

fn ix(name: &str, desc: bool) -> IndexColumn {
    IndexColumn { name: name.into(), descending: desc }
}

fn assert_fixture_shape(s: &Schema, db: &str) {
    assert_eq!(s.engine, Some(Engine::Mysql));
    assert_eq!(s.database, db);
    let names: Vec<&str> = s.tables.iter().map(|t| t.name.name.as_str()).collect();
    assert_eq!(names, ["customers", "order_items"]);
    assert!(s.tables.iter().all(|t| t.name.schema.is_empty()));

    let c = &s.tables[0];
    let col = |n: &str| c.column(n).unwrap_or_else(|| panic!("column {n}"));
    let id = col("id");
    assert_eq!((id.data_type.as_str(), id.nullable, id.default.as_deref()), ("int unsigned", false, None));
    assert_eq!(id.identity, Some(Identity::default()));
    assert_eq!(col("email").collation, None);
    assert_eq!(col("name").collation.as_deref(), Some("utf8mb4_bin"));
    assert_eq!(col("name").default.as_deref(), Some(r"'it''s \\ x'"));
    assert_eq!(col("status").data_type, "enum('Active','blocked')");
    assert_eq!(col("status").default.as_deref(), Some("'Active'"));
    assert_eq!(col("flag").data_type, "tinyint(1)");
    assert_eq!(col("flag").default.as_deref(), Some("0"));
    assert_eq!(col("score").default.as_deref(), Some("1.50"));
    assert_eq!(col("bio").default, None);
    assert_eq!(col("created_at").default.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(
        col("updated_at").default.as_deref(),
        Some("CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)")
    );
    assert_eq!(col("touched").default.as_deref(), Some("ON UPDATE CURRENT_TIMESTAMP"));
    assert_eq!(col("joined").default.as_deref(), Some("(curdate())"));
    assert_eq!(col("name_len").computed.as_deref(), Some("(char_length(`name`)) VIRTUAL"));
    assert_eq!(col("email_lc").computed.as_deref(), Some("(lower(`email`)) STORED"));
    assert_eq!(col("email_lc").default, None);

    assert_eq!(c.primary_key.as_ref().map(|p| p.columns.clone()), Some(vec!["id".to_string()]));
    let idx: Vec<&str> = c.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(idx, ["ft_bio", "ix_lower_name", "ix_name_prefix", "uq_email"]);
    assert_eq!(c.indexes[0].method.as_deref(), Some("fulltext"));
    assert_eq!(c.indexes[1].columns, vec![ix("lower(`name`)", false)]);
    assert_eq!(c.indexes[2].columns, vec![ix("name(10)", false), ix("status", true)]);
    assert!(!c.indexes[2].unique && !c.indexes[2].constraint && c.indexes[2].method.is_none());
    assert!(c.indexes[3].unique && c.indexes[3].constraint);
    assert_eq!(c.checks, vec![CheckConstraint { name: "ck_score".into(), expression: "(`score` >= 0)".into() }]);

    let o = &s.tables[1];
    let pk = o.primary_key.as_ref().expect("composite pk");
    assert_eq!(pk.columns, ["order_id", "line"]);
    assert_eq!(
        o.foreign_keys,
        vec![ForeignKey {
            name: "fk_items_customer".into(),
            columns: vec!["customer_id".into()],
            ref_table: ObjectName::new("", "customers"),
            ref_columns: vec!["id".into()],
            on_delete: Some("CASCADE".into()),
            on_update: None,
        }]
    );

    let objs: Vec<(ObjectKind, &str)> = s.objects.iter().map(|o| (o.kind, o.name.name.as_str())).collect();
    assert_eq!(
        objs,
        [
            (ObjectKind::Function, "f_double"),
            (ObjectKind::Procedure, "p_count"),
            (ObjectKind::Trigger, "trg_customers_bi"),
            (ObjectKind::View, "v_active"),
        ]
    );
    for o in &s.objects {
        assert!(!o.definition.contains("DEFINER"), "{}", o.definition);
        assert!(!o.definition.contains(&format!("`{db}`")), "{}", o.definition);
    }
    let view = &s.objects[3];
    assert!(view.definition.starts_with("CREATE VIEW `v_active` AS select "), "{}", view.definition);
    assert!(s.objects[1].definition.starts_with("CREATE PROCEDURE `p_count`(OUT n int)"));
    assert_eq!(s.objects[2].table, Some(ObjectName::new("", "customers")));
}

#[tokio::test]
async fn schema_introspection_and_round_trip() {
    let Some(cfg) = config() else { return };
    let (a, b, c) = ("pcdb_schema_a", "pcdb_schema_b", "pcdb_schema_c");
    fresh(&cfg, &[a, b, c]).await;
    exec(&with_db(&cfg, a), FIXTURE).await;
    exec(&with_db(&cfg, b), FIXTURE).await;

    let sa = load(&with_db(&cfg, a)).await;
    let sb = load(&with_db(&cfg, b)).await;
    assert_fixture_shape(&sa, a);
    assert_eq!(sa.tables, sb.tables);
    assert_eq!(sa.objects, sb.objects);

    // Recreate A's model in C through the dialect and re-introspect.
    let d = dialect(Engine::Mysql);
    let mut stmts = Vec::new();
    for t in &sa.tables {
        stmts.push(d.create_table(t));
        stmts.extend(t.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
    }
    for t in &sa.tables {
        stmts.extend(t.foreign_keys.iter().map(|fk| d.add_foreign_key(&t.name, fk)));
    }
    for kind in [ObjectKind::View, ObjectKind::Function, ObjectKind::Procedure, ObjectKind::Trigger] {
        for o in sa.objects.iter().filter(|o| o.kind == kind) {
            stmts.extend(d.create_object(o));
        }
    }
    Client::connect(&with_db(&cfg, c))
        .await
        .expect("connect")
        .execute(&stmts, false)
        .await
        .unwrap_or_else(|e| panic!("recreate failed: {e}\n{}", stmts.join(";\n")));
    let sc = load(&with_db(&cfg, c)).await;
    assert_eq!(sa.tables, sc.tables);
    assert_eq!(sa.objects, sc.objects);

    // Alter + replace statements execute too.
    let customers = &sa.tables[0];
    let mut to = customers.column("updated_at").unwrap().clone();
    to.nullable = false;
    let mut alter = d.alter_column(&customers.name, customers.column("updated_at").unwrap(), &to);
    alter.extend(d.replace_object(&sa.objects[3], &sa.objects[3]));
    alter.extend(d.replace_object(&sa.objects[1], &sa.objects[1]));
    alter.push(d.drop_index(&customers.name, &customers.indexes[3]));
    alter.push(d.create_index(&customers.name, &customers.indexes[3]));
    alter.push(d.drop_check(&customers.name, &customers.checks[0]));
    alter.push(d.add_check(&customers.name, &customers.checks[0]));
    Client::connect(&with_db(&cfg, c)).await.unwrap().execute(&alter, false).await.expect("alter");
    let sc = load(&with_db(&cfg, c)).await;
    let updated = sc.tables[0].column("updated_at").unwrap();
    assert!(!updated.nullable);
    assert_eq!(updated.default, to.default);
    assert_eq!(sc.objects, sa.objects);
    assert_eq!(sc.tables[0].indexes, customers.indexes);
    assert_eq!(sc.tables[0].checks, customers.checks);

    drop_dbs(&cfg, &[a, b, c]).await;
}

const TYPES_TABLE: &str = "CREATE TABLE all_types (
    id int NOT NULL PRIMARY KEY,
    ti tinyint, tu tinyint unsigned, si smallint, mi mediumint, i int, iu int unsigned,
    bi bigint, bu bigint unsigned, b1 tinyint(1), bt bit(10),
    de decimal(12,4), fl float, db double,
    ch char(5), vc varchar(20), tx text, en enum('x','y'), st set('a','b','c'),
    bn binary(3), vb varbinary(10), bl blob,
    d date, t time(3), dt datetime(6), ts timestamp(2) NULL, y year, j json,
    g geometry, p point
)";

const COLUMNS: &[&str] = &[
    "id", "ti", "tu", "si", "mi", "i", "iu", "bi", "bu", "b1", "bt", "de", "fl", "db", "ch", "vc", "tx", "en", "st",
    "bn", "vb", "bl", "d", "t", "dt", "ts", "y", "j", "g", "p",
];

#[tokio::test]
async fn fetch_rows_canonical_values() {
    let Some(cfg) = config() else { return };
    let db = "pcdb_rows";
    fresh(&cfg, &[db]).await;
    let cfg_db = with_db(&cfg, db);
    exec(
        &cfg_db,
        &[
            TYPES_TABLE,
            r#"INSERT INTO all_types VALUES (1, -128, 255, -32768, -8388608, -2147483648, 4294967295,
                -9223372036854775808, 18446744073709551615, 1, b'1000000001',
                12.5000, 1.5, 2.25, 'ab', 'it''s \\ x', 'long text', 'y', 'c,a',
                0x010203, 0x00ff, 0xdeadbeef,
                '2024-02-29', '-12:34:56.500', '2024-01-02 03:04:05.123400', '2024-06-01 12:00:00.50', 2024,
                '{"b": 1, "a": [1, 2]}', ST_GeomFromText('LINESTRING(0 0,1 1)'), POINT(1, 2))"#,
            "INSERT INTO all_types (id) VALUES (2)",
            "SET SESSION sql_mode = ''",
            "INSERT INTO all_types (id, d, dt, ts) VALUES (3, '0000-00-00', '0000-00-00 00:00:00', '0000-00-00 00:00:00')",
        ],
    )
    .await;

    let columns: Vec<String> = COLUMNS.iter().map(|c| c.to_string()).collect();
    let table = ObjectName::new("", "all_types");
    let mut client = Client::connect(&cfg_db).await.unwrap();
    let rows = client.fetch_rows(&table, &columns, &["id".into()]).await.expect("fetch");
    assert_eq!(rows.len(), 3);

    use Value::*;
    let r = &rows[0];
    let expected = vec![
        Int(1),
        Int(-128),
        Int(255),
        Int(-32768),
        Int(-8388608),
        Int(-2147483648),
        Int(4294967295),
        Int(i64::MIN),
        UInt(u64::MAX),
        Int(1),
        Int(513),
        Decimal("12.5".into()),
        Float(1.5),
        Float(2.25),
        Text("ab".into()),
        Text(r"it's \ x".into()),
        Text("long text".into()),
        Text("y".into()),
        Text("a,c".into()),
        Bytes(vec![1, 2, 3]),
        Bytes(vec![0, 255]),
        Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        Date("2024-02-29".into()),
        Time("-12:34:56.5".into()),
        DateTime("2024-01-02 03:04:05.1234".into()),
        DateTime("2024-06-01 12:00:00.5".into()),
        Int(2024),
    ];
    assert_eq!(r[..expected.len()], expected[..]);
    let Document(json) = &r[27] else { panic!("json: {:?}", r[27]) };
    let json: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(json, serde_json::json!({"a": [1, 2], "b": 1}));
    assert_eq!(r[28], Text("LINESTRING(0 0,1 1)".into()));
    assert_eq!(r[29], Text("POINT(1 2)".into()));

    assert_eq!(rows[1][0], Int(2));
    assert!(rows[1][1..].iter().all(Value::is_null), "{:?}", rows[1]);

    let z = &rows[2];
    assert_eq!(z[22], Text("0000-00-00".into()));
    assert_eq!(z[24], Text("0000-00-00 00:00:00".into()));
    assert_eq!(z[25], Text("0000-00-00 00:00:00".into()));

    // Every value survives a dialect INSERT round trip.
    let d = dialect(Engine::Mysql);
    let copy = ObjectName::new("", "all_types_copy");
    let mut stmts = vec!["SET SESSION sql_mode = ''".to_string(), "CREATE TABLE all_types_copy LIKE all_types".into()];
    for row in &rows {
        let (cols, vals): (Vec<String>, Vec<Value>) = columns
            .iter()
            .zip(row)
            // Geometry comes back as WKT text; it can't be inserted verbatim.
            .filter(|(c, _)| !matches!(c.as_str(), "g" | "p"))
            .map(|(c, v)| (c.clone(), v.clone()))
            .unzip();
        stmts.push(d.insert(&copy, &cols, &vals));
    }
    client.execute(&stmts, true).await.expect("insert copy");
    let back = client.fetch_rows(&copy, &columns[..28], &["id".into()]).await.unwrap();
    for (orig, back) in rows.iter().zip(&back) {
        assert_eq!(orig[..28], back[..], "dialect literal round trip");
    }

    drop_dbs(&cfg, &[db]).await;
}

#[tokio::test]
async fn execute_rolls_back_and_lists_databases() {
    let Some(cfg) = config() else { return };
    let db = "pcdb_exec";
    fresh(&cfg, &[db]).await;
    let cfg_db = with_db(&cfg, db);
    exec(&cfg_db, &["CREATE TABLE t (id int PRIMARY KEY, v varchar(10))"]).await;

    let mut client = Client::connect(&cfg_db).await.unwrap();
    let stmts = vec!["INSERT INTO t VALUES (1, 'a')".to_string(), "INSERT INTO t VALUES (1, 'dup')".into()];
    let err = client.execute(&stmts, true).await.expect_err("duplicate key").to_string();
    assert!(err.contains("statement 2 failed"), "{err}");
    let rows = client.fetch_rows(&ObjectName::new("", "t"), &["id".into()], &["id".into()]).await.unwrap();
    assert!(rows.is_empty(), "rolled back: {rows:?}");

    let dbs = client.list_databases().await.unwrap();
    assert!(dbs.contains(&db.to_string()));
    assert!(!dbs.iter().any(|d| ["mysql", "sys", "information_schema", "performance_schema"].contains(&d.as_str())));
    let version = client.server_version().await.unwrap();
    assert!(version.starts_with("MySQL ") || version.starts_with("MariaDB "), "{version}");

    // No database selected: listing works, schema loading explains itself.
    let mut bare = Client::connect(&with_db(&cfg, "")).await.unwrap();
    assert!(bare.list_databases().await.unwrap().contains(&db.to_string()));
    let err = bare.load_schema().await.expect_err("no database").to_string();
    assert!(err.contains("no database selected"), "{err}");

    drop_dbs(&cfg, &[db]).await;
}

/// The session's negotiated TLS version (`""` = plaintext), read from
/// `performance_schema.session_status`; `None` when that isn't available
/// (e.g. MariaDB with performance_schema off).
async fn ssl_version(c: &mut Client) -> Option<String> {
    let table = ObjectName::new("performance_schema", "session_status");
    let rows = c.fetch_rows(&table, &["VARIABLE_NAME".into(), "VARIABLE_VALUE".into()], &[]).await.ok()?;
    rows.into_iter().find(|r| r[0].to_string() == "Ssl_version").map(|r| r[1].to_string())
}

#[tokio::test]
async fn connects_with_each_tls_mode() {
    let Some(cfg) = config() else { return };
    let mut versions = Vec::new();
    for tls in [TlsMode::Disable, TlsMode::Prefer, TlsMode::Require] {
        let mut c = match Client::connect(&ConnectionConfig { tls, ..cfg.clone() }).await {
            Ok(c) => c,
            Err(e) if tls == TlsMode::Require => panic!("server has no TLS? connect with Require: {e}"),
            Err(e) => panic!("connect with {tls:?}: {e}"),
        };
        c.server_version().await.unwrap();
        versions.push(ssl_version(&mut c).await);
    }
    if let [Some(off), Some(prefer), Some(require)] = &versions[..] {
        assert_eq!(off, "", "Disable must not encrypt");
        assert!(require.starts_with("TLS"), "Require negotiated {require:?}");
        assert_eq!(prefer, require, "Prefer uses TLS when the server offers it");
    }
}
