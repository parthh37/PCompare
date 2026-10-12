//! PostgreSQL driver + dialect against a live server.
//!
//! Skipped unless `PCOMPARE_LIVE_POSTGRES` holds a JSON `ConnectionConfig`
//! for a superuser, e.g.
//! `{"engine":"postgres","host":"127.0.0.1","port":55432,"user":"postgres","database":"postgres"}`.
//! Creates (and drops) the scratch databases `pcompare_live_a` / `_b`.

use pcompare_db::connection::ConnectionConfig;
use pcompare_db::dialect::dialect;
use pcompare_db::driver::Client;
use pcompare_db::model::*;
use pcompare_db::value::Value;

const DB_A: &str = "pcompare_live_a";
const DB_B: &str = "pcompare_live_b";

/// `app`: the schema-compare fixture. `data`: types for fetch_rows.
const FIXTURE: &str = r#"
CREATE EXTENSION citext;
CREATE SCHEMA app;
CREATE SCHEMA data;
CREATE TABLE public.misc (id int);
CREATE SEQUENCE app.invoice_no AS integer START WITH 1000 INCREMENT BY 10;
CREATE TABLE app.customers (
    id integer GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY 5) PRIMARY KEY,
    email text COLLATE "C" NOT NULL,
    name varchar(80) DEFAULT 'anon',
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT customers_email_key UNIQUE (email)
);
CREATE TABLE app.orders (
    id serial PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES app.customers (id) ON DELETE CASCADE,
    qty integer NOT NULL CONSTRAINT orders_qty_check CHECK (qty > 0),
    price numeric(10,2) NOT NULL,
    total numeric GENERATED ALWAYS AS (qty * price) STORED,
    deleted_at timestamp,
    invoice integer DEFAULT nextval('app.invoice_no')
);
CREATE TABLE app.order_lines (
    order_id integer REFERENCES app.orders ON UPDATE SET NULL,
    line_no smallint,
    sku text,
    PRIMARY KEY (order_id, line_no)
);
CREATE INDEX orders_live_idx ON app.orders (customer_id, id DESC) INCLUDE (qty) WHERE deleted_at IS NULL;
CREATE INDEX customers_lower_email_idx ON app.customers (lower(email));
CREATE INDEX customers_name_hash ON app.customers USING hash (name);
CREATE VIEW app.big_orders AS SELECT id, total FROM app.orders WHERE total > 100;
CREATE MATERIALIZED VIEW app.order_totals AS SELECT customer_id, sum(total) AS total FROM app.orders GROUP BY customer_id;
CREATE FUNCTION app.order_total(o integer) RETURNS numeric LANGUAGE sql
    AS $$ SELECT total FROM app.orders WHERE id = o $$;
CREATE FUNCTION app.order_total(o integer, tax numeric) RETURNS numeric LANGUAGE sql
    AS $$ SELECT total * (1 + tax) FROM app.orders WHERE id = o $$;
CREATE PROCEDURE app.purge() LANGUAGE sql AS $$ DELETE FROM app.orders WHERE deleted_at IS NOT NULL $$;
CREATE FUNCTION app.touch() RETURNS trigger LANGUAGE plpgsql
    AS $$ BEGIN NEW.deleted_at := NULL; RETURN NEW; END $$;
CREATE TRIGGER orders_touch BEFORE INSERT ON app.orders FOR EACH ROW EXECUTE FUNCTION app.touch();

CREATE TYPE data.mood AS ENUM ('happy', 'sad');
CREATE DOMAIN data.posint AS integer CHECK (VALUE > 0);
CREATE TABLE data.all_types (
    id int8 PRIMARY KEY, i2 int2, i4 int4, n numeric(12,4), nn numeric, f4 real, f8 double precision,
    b bool, t text, vc varchar(10), c char(5), nm name, by bytea, d date, tm time, ttz timetz,
    ts timestamp, tstz timestamptz, u uuid, j json, jb jsonb, iv interval, ip inet, arr int[],
    mood data.mood, ci citext, dom data.posint
);
INSERT INTO data.all_types VALUES
    (1, -32768, 2147483647, 12.5000, 'NaN', 1.1, 2.5, true, 'hi', 'v', 'ab', 'nm', '\x00ff',
     '2024-01-02', '10:00:00.250', '10:00:00+05:30', '2024-01-02 10:00:00.500',
     '2024-01-02 15:30:00+05:30', 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', '{"b": 1, "a": 2}',
     '{"b": 1, "a": 2}', '1 day 02:00:00', '10.0.0.1', '{1,2}', 'happy', 'MiXed', 5),
    (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
     NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
"#;

fn live_config() -> Option<ConnectionConfig> {
    let json = std::env::var("PCOMPARE_LIVE_POSTGRES").ok()?;
    Some(serde_json::from_str(&json).expect("PCOMPARE_LIVE_POSTGRES must be a ConnectionConfig JSON"))
}

fn with_db(cfg: &ConnectionConfig, db: &str, schemas: &[&str]) -> ConnectionConfig {
    ConnectionConfig { database: db.into(), schemas: schemas.iter().map(|s| s.to_string()).collect(), ..cfg.clone() }
}

async fn recreate(admin: &mut Client, db: &str) {
    let stmts = vec![format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"), format!("CREATE DATABASE {db}")];
    admin.execute(&stmts, false).await.unwrap();
}

fn table<'a>(s: &'a Schema, schema: &str, name: &str) -> &'a Table {
    s.tables.iter().find(|t| t.name == ObjectName::new(schema, name)).unwrap_or_else(|| panic!("no table {schema}.{name}"))
}

fn object<'a>(s: &'a Schema, kind: ObjectKind, name: &str) -> &'a CodeObject {
    s.objects.iter().find(|o| o.kind == kind && o.name.name == name).unwrap_or_else(|| panic!("no {kind:?} {name}"))
}

#[tokio::test]
async fn live_postgres() {
    let Some(cfg) = live_config() else {
        eprintln!("PCOMPARE_LIVE_POSTGRES not set; skipping live PostgreSQL test");
        return;
    };
    let mut admin = Client::connect(&cfg).await.unwrap();
    assert!(admin.server_version().await.unwrap().starts_with("PostgreSQL "));
    recreate(&mut admin, DB_A).await;
    recreate(&mut admin, DB_B).await;
    let dbs = admin.list_databases().await.unwrap();
    assert!(dbs.contains(&DB_A.to_owned()) && !dbs.iter().any(|d| d.starts_with("template")));

    let mut a = Client::connect(&with_db(&cfg, DB_A, &[])).await.unwrap();
    a.execute(&[FIXTURE.to_owned()], true).await.unwrap();

    introspection(&mut a).await;
    fetch_rows(&mut a).await;
    rollback(&mut a).await;
    round_trip(&cfg).await;

    drop(a);
    let stmts = vec![
        format!("DROP DATABASE IF EXISTS {DB_A} WITH (FORCE)"),
        format!("DROP DATABASE IF EXISTS {DB_B} WITH (FORCE)"),
    ];
    admin.execute(&stmts, false).await.unwrap();
}

async fn introspection(a: &mut Client) {
    let s = a.load_schema().await.unwrap();
    assert_eq!(s.engine, Some(Engine::Postgres));
    assert_eq!(s.database, DB_A);
    // Every user schema, nothing from pg_catalog or the citext extension.
    let names: Vec<String> = s.tables.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["app.customers", "app.order_lines", "app.orders", "data.all_types", "public.misc"]);
    assert!(!s.objects.iter().any(|o| o.name.schema == "public"), "extension objects leaked: {:?}", s.objects);

    let customers = table(&s, "app", "customers");
    let id = customers.column("id").unwrap();
    assert_eq!(id.data_type, "integer");
    assert_eq!(id.identity, Some(Identity { seed: 100, increment: 5 }));
    assert!(!id.nullable && id.default.is_none());
    assert_eq!(customers.column("email").unwrap().collation.as_deref(), Some("C"));
    assert_eq!(customers.column("name").unwrap().data_type, "character varying(80)");
    assert_eq!(customers.column("name").unwrap().collation, None);
    assert_eq!(customers.column("name").unwrap().default.as_deref(), Some("'anon'::character varying"));
    assert_eq!(customers.column("created_at").unwrap().data_type, "timestamp with time zone");
    let pk = customers.primary_key.as_ref().unwrap();
    assert_eq!((pk.name.as_deref(), pk.columns.as_slice()), (Some("customers_pkey"), &["id".to_owned()][..]));
    let idx: Vec<(&str, bool, bool, Option<&str>)> =
        customers.indexes.iter().map(|i| (i.name.as_str(), i.unique, i.constraint, i.method.as_deref())).collect();
    assert_eq!(
        idx,
        [
            ("customers_email_key", true, true, None),
            ("customers_lower_email_idx", false, false, None),
            ("customers_name_hash", false, false, Some("hash")),
        ]
    );
    assert_eq!(customers.indexes[1].columns[0].name, "lower(email)");

    let orders = table(&s, "app", "orders");
    let id = orders.column("id").unwrap();
    assert_eq!((id.identity.clone(), id.default.clone()), (Some(Identity::default()), None)); // serial
    assert_eq!(orders.column("invoice").unwrap().default.as_deref(), Some("nextval('app.invoice_no'::regclass)"));
    assert_eq!(orders.column("invoice").unwrap().identity, None);
    let total = orders.column("total").unwrap();
    assert_eq!((total.computed.as_deref(), total.default.as_deref()), (Some("qty::numeric * price"), None));
    assert_eq!(orders.checks, [CheckConstraint { name: "orders_qty_check".into(), expression: "qty > 0".into() }]);
    let fk = &orders.foreign_keys[0];
    assert_eq!(fk.name, "orders_customer_id_fkey");
    assert_eq!((fk.ref_table.clone(), fk.ref_columns.clone()), (ObjectName::new("app", "customers"), vec!["id".to_owned()]));
    assert_eq!((fk.on_delete.as_deref(), fk.on_update.as_deref()), (Some("CASCADE"), None));
    let live = &orders.indexes[0];
    assert_eq!(live.name, "orders_live_idx");
    let cols: Vec<(&str, bool)> = live.columns.iter().map(|c| (c.name.as_str(), c.descending)).collect();
    assert_eq!(cols, [("customer_id", false), ("id", true)]);
    assert_eq!(live.include, ["qty"]);
    assert_eq!(live.filter.as_deref(), Some("deleted_at IS NULL"));

    let lines = table(&s, "app", "order_lines");
    assert_eq!(lines.primary_key.as_ref().unwrap().columns, ["order_id", "line_no"]);
    assert_eq!(lines.foreign_keys[0].on_update.as_deref(), Some("SET NULL"));
    assert!(lines.column("line_no").unwrap().data_type == "smallint" && !lines.column("line_no").unwrap().nullable);

    let kinds: Vec<(ObjectKind, String)> =
        s.objects.iter().filter(|o| o.name.schema == "app").map(|o| (o.kind, o.name.name.clone())).collect();
    assert_eq!(
        kinds,
        [
            (ObjectKind::View, "big_orders".into()),
            (ObjectKind::Sequence, "invoice_no".into()), // orders_id_seq (serial) is not listed
            (ObjectKind::Function, "order_total(integer)".into()),
            (ObjectKind::Function, "order_total(integer, numeric)".into()),
            (ObjectKind::View, "order_totals".into()),
            (ObjectKind::Trigger, "orders.orders_touch".into()),
            (ObjectKind::Procedure, "purge()".into()),
            (ObjectKind::Function, "touch()".into()),
        ]
    );
    let view = object(&s, ObjectKind::View, "big_orders");
    assert!(view.definition.starts_with("CREATE VIEW app.big_orders AS\nSELECT orders.id"), "{}", view.definition);
    assert!(object(&s, ObjectKind::View, "order_totals").definition.starts_with("CREATE MATERIALIZED VIEW app.order_totals AS"));
    let seq = object(&s, ObjectKind::Sequence, "invoice_no");
    assert_eq!(
        seq.definition,
        "CREATE SEQUENCE app.invoice_no AS integer START WITH 1000 INCREMENT BY 10 MINVALUE 1 MAXVALUE 2147483647"
    );
    let f = object(&s, ObjectKind::Function, "order_total(integer)");
    assert!(f.definition.starts_with("CREATE OR REPLACE FUNCTION app.order_total(o integer)\n RETURNS numeric"));
    let trg = object(&s, ObjectKind::Trigger, "orders.orders_touch");
    assert_eq!(trg.table, Some(ObjectName::new("app", "orders")));
    assert_eq!(
        trg.definition,
        "CREATE TRIGGER orders_touch BEFORE INSERT ON app.orders FOR EACH ROW EXECUTE FUNCTION app.touch()"
    );
    assert!(object(&s, ObjectKind::Procedure, "purge()").definition.starts_with("CREATE OR REPLACE PROCEDURE app.purge()"));
}

const TYPE_COLUMNS: &[&str] = &[
    "id", "i2", "i4", "n", "nn", "f4", "f8", "b", "t", "vc", "c", "nm", "by", "d", "tm", "ttz", "ts", "tstz", "u", "j",
    "jb", "iv", "ip", "arr", "mood", "ci", "dom",
];

async fn fetch_rows(a: &mut Client) {
    let t = ObjectName::new("data", "all_types");
    let cols: Vec<String> = TYPE_COLUMNS.iter().map(|s| s.to_string()).collect();
    let rows = a.fetch_rows(&t, &cols, &cols[..1]).await.unwrap();
    assert_eq!(rows.len(), 2);
    let text = |s: &str| Value::Text(s.into());
    let expected = vec![
        Value::Int(1),
        Value::Int(-32768),
        Value::Int(2147483647),
        Value::Decimal("12.5".into()),
        text("NaN"),
        Value::Float(1.1),
        Value::Float(2.5),
        Value::Bool(true),
        text("hi"),
        text("v"),
        text("ab   "),
        text("nm"),
        Value::Bytes(vec![0, 255]),
        Value::Date("2024-01-02".into()),
        Value::Time("10:00:00.25".into()),
        Value::Time("10:00:00+05:30".into()),
        Value::DateTime("2024-01-02 10:00:00.5".into()),
        Value::DateTimeTz("2024-01-02 10:00:00+00:00".into()),
        Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
        Value::Document("{\"b\": 1, \"a\": 2}".into()),
        Value::Document("{\"a\": 2, \"b\": 1}".into()),
        text("1 day 02:00:00"),
        text("10.0.0.1/32"), // inet::text keeps the mask
        text("{1,2}"),
        text("happy"),
        text("MiXed"),
        Value::Int(5),
    ];
    assert_eq!(rows[0], expected);
    assert!(rows[1][1..].iter().all(Value::is_null));

    // Dialect literals for every canonical value insert back unchanged.
    let mut copy = expected.clone();
    copy[0] = Value::Int(3);
    let insert = dialect(Engine::Postgres).insert(&t, &cols, &copy);
    a.execute(&[insert], true).await.unwrap();
    let back = a.fetch_rows(&t, &cols, &cols[..1]).await.unwrap();
    assert_eq!(back[2], copy);
}

async fn rollback(a: &mut Client) {
    let stmts = vec![
        "INSERT INTO app.customers (email) VALUES ('x@example.com')".to_owned(),
        "CREATE TABLE app.scratch (x int)".to_owned(),
        "INSERT INTO app.nope VALUES (1)".to_owned(),
    ];
    let e = a.execute(&stmts, true).await.unwrap_err().to_string();
    assert!(e.contains("statement 3 failed") && e.contains("app.nope"), "{e}");
    let s = a.load_schema().await.unwrap();
    assert!(!s.tables.iter().any(|t| t.name.name == "scratch"));
    let rows = a.fetch_rows(&ObjectName::new("app", "customers"), &["id".into()], &[]).await.unwrap();
    assert!(rows.is_empty(), "partial changes survived: {rows:?}");
}

/// Recreates schema `app` from its model through the dialect in a second
/// database and checks the result introspects identically.
async fn round_trip(cfg: &ConnectionConfig) {
    let mut a = Client::connect(&with_db(cfg, DB_A, &["app"])).await.unwrap();
    let source = a.load_schema().await.unwrap();
    assert!(source.tables.iter().all(|t| t.name.schema == "app"), "schema filter ignored");

    let d = dialect(Engine::Postgres);
    let by_kind = |k: ObjectKind| source.objects.iter().filter(move |o| o.kind == k);
    let mut stmts = vec!["CREATE SCHEMA app".to_owned()];
    stmts.extend(by_kind(ObjectKind::Sequence).flat_map(|o| d.create_object(o)));
    stmts.extend(source.tables.iter().map(|t| d.create_table(t)));
    for t in &source.tables {
        stmts.extend(t.foreign_keys.iter().map(|fk| d.add_foreign_key(&t.name, fk)));
        stmts.extend(t.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
    }
    for k in [ObjectKind::Function, ObjectKind::Procedure, ObjectKind::View, ObjectKind::Trigger] {
        stmts.extend(by_kind(k).flat_map(|o| d.create_object(o)));
    }
    let mut b = Client::connect(&with_db(cfg, DB_B, &["app"])).await.unwrap();
    b.execute(&stmts, true).await.unwrap();
    let mut copy = b.load_schema().await.unwrap();
    // The serial column was recreated as an identity column; both read the same.
    copy.database = source.database.clone();
    assert_eq!(copy.tables, source.tables);
    assert_eq!(copy.objects, source.objects);

    // Replacing objects in place works on a live server.
    let f = object(&source, ObjectKind::Function, "order_total(integer)");
    let mut f2 = f.clone();
    f2.definition = f.definition.replace("SELECT total", "SELECT total + 0");
    let seq = object(&source, ObjectKind::Sequence, "invoice_no");
    let mut seq2 = seq.clone();
    seq2.definition = seq.definition.replace("INCREMENT BY 10", "INCREMENT BY 20");
    let trg = object(&source, ObjectKind::Trigger, "orders.orders_touch");
    let mut stmts = d.replace_object(f, &f2);
    stmts.extend(d.replace_object(seq, &seq2));
    stmts.extend(d.replace_object(trg, trg));
    stmts.push(d.drop_object(object(&source, ObjectKind::Function, "order_total(integer, numeric)")));
    stmts.push(d.drop_object(object(&source, ObjectKind::View, "order_totals")));
    b.execute(&stmts, true).await.unwrap();
    let after = b.load_schema().await.unwrap();
    assert_eq!(object(&after, ObjectKind::Function, "order_total(integer)").definition, f2.definition);
    assert_eq!(object(&after, ObjectKind::Sequence, "invoice_no").definition, seq2.definition);
    assert!(!after.objects.iter().any(|o| o.name.name == "order_total(integer, numeric)" || o.name.name == "order_totals"));
}
