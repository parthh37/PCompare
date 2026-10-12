//! Live SQL Server tests. Skipped unless `PCOMPARE_LIVE_MSSQL` holds a JSON
//! `ConnectionConfig` for a login allowed to create databases, e.g.
//!
//! ```text
//! docker run -d --name pcompare-mssql -e ACCEPT_EULA=Y -e 'MSSQL_SA_PASSWORD=Pcompare!2024' \
//!     -p 1433:1433 mcr.microsoft.com/mssql/server:2022-latest
//! PCOMPARE_LIVE_MSSQL='{"engine":"mssql","host":"127.0.0.1","port":1433,"user":"sa",
//!     "password":"Pcompare!2024","trustServerCertificate":true,"database":"master"}' \
//!     cargo test -p pcompare-db --test live_mssql -- --nocapture
//! ```
//!
//! Creates two scratch databases, introspects a realistic fixture in the
//! first, replays it into the second through the dialect (round trip), and
//! checks canonical row values and transactional execution. Both databases
//! are dropped afterwards, even when an assertion fails.

use pcompare_db::connection::ConnectionConfig;
use pcompare_db::dialect::dialect;
use pcompare_db::driver::Client;
use pcompare_db::model::*;
use pcompare_db::value::Value;

fn live_config() -> Option<ConnectionConfig> {
    let json = std::env::var("PCOMPARE_LIVE_MSSQL").ok()?;
    let mut cfg: ConnectionConfig = serde_json::from_str(&json).expect("PCOMPARE_LIVE_MSSQL is not a ConnectionConfig");
    cfg.engine = Some(Engine::Mssql);
    Some(cfg)
}

fn with_db(base: &ConnectionConfig, db: &str) -> ConnectionConfig {
    ConnectionConfig { database: db.into(), ..base.clone() }
}

async fn connect(cfg: &ConnectionConfig) -> Client {
    Client::connect(cfg).await.unwrap_or_else(|e| panic!("connect {}: {e}", cfg.describe()))
}

async fn exec(c: &mut Client, stmts: &[&str]) {
    let owned: Vec<String> = stmts.iter().map(|s| s.to_string()).collect();
    c.execute(&owned, false).await.unwrap_or_else(|e| panic!("{e}"));
}

#[tokio::test]
async fn live_mssql() {
    let Some(base) = live_config() else {
        eprintln!("PCOMPARE_LIVE_MSSQL not set; skipping live SQL Server tests");
        return;
    };
    let id = std::process::id();
    let (db_a, db_b) = (format!("pcompare_live_a_{id}"), format!("pcompare_live_b_{id}"));

    let mut admin = connect(&with_db(&base, "master")).await;
    println!("server: {}", admin.server_version().await.unwrap());
    exec(&mut admin, &[&format!("CREATE DATABASE [{db_a}]"), &format!("CREATE DATABASE [{db_b}]")]).await;

    // Run in a task so a failed assertion still reaches the cleanup below.
    let outcome = tokio::spawn(run(base.clone(), db_a.clone(), db_b.clone())).await;

    for db in [&db_a, &db_b] {
        let drop = format!(
            "IF DB_ID(N'{db}') IS NOT NULL BEGIN \
             ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END"
        );
        if let Err(e) = admin.execute(&[drop], false).await {
            eprintln!("cleanup of {db} failed: {e}");
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn run(base: ConnectionConfig, db_a: String, db_b: String) {
    let cfg_a = with_db(&base, &db_a);
    let cfg_b = with_db(&base, &db_b);

    let mut admin = connect(&with_db(&base, "master")).await;
    let dbs = admin.list_databases().await.unwrap();
    assert!(dbs.contains(&db_a) && dbs.contains(&db_b), "{dbs:?}");
    assert!(!dbs.iter().any(|d| d == "master" || d == "tempdb"), "system databases listed: {dbs:?}");

    let mut a = connect(&cfg_a).await;
    exec(&mut a, FIXTURE).await;
    exec(&mut a, DATA).await;

    let schema_a = a.load_schema().await.unwrap();
    check_model(&schema_a, &db_a);
    check_schema_filter(&cfg_a).await;
    check_rows(&mut a).await;
    check_execute(&mut a).await;

    let mut b = connect(&cfg_b).await;
    round_trip(&mut a, &schema_a, &mut b).await;
    check_alters(&mut b).await;
}

/// The fixture: one statement per batch, as `execute` requires.
const FIXTURE: &[&str] = &[
    "CREATE SCHEMA sales",
    "CREATE SEQUENCE dbo.OrderNumbers AS bigint START WITH 1000 INCREMENT BY 5",
    "CREATE TABLE dbo.Customers (
        CustomerId int IDENTITY(10,2) NOT NULL CONSTRAINT PK_Customers PRIMARY KEY CLUSTERED,
        Email nvarchar(200) NOT NULL,
        Name nvarchar(100) COLLATE Latin1_General_CS_AS NULL,
        Status char(1) NOT NULL CONSTRAINT DF_Customers_Status DEFAULT ('A'),
        CreatedAt datetime2(3) NOT NULL CONSTRAINT DF_Customers_CreatedAt DEFAULT (sysutcdatetime()),
        Notes nvarchar(max) NULL,
        RowVer rowversion NOT NULL,
        CONSTRAINT UQ_Customers_Email UNIQUE NONCLUSTERED (Email)
    )",
    "CREATE NONCLUSTERED INDEX IX_Customers_Status ON dbo.Customers (Status DESC, Email) INCLUDE (Name) WHERE Status <> 'X'",
    "CREATE TABLE dbo.Orders (
        CustomerId int NOT NULL,
        OrderNo int NOT NULL,
        Qty int NOT NULL CONSTRAINT DF_Orders_Qty DEFAULT ((1)),
        Price decimal(10,2) NOT NULL,
        Total AS (Qty * Price) PERSISTED,
        Discount AS (Price / 10),
        CONSTRAINT PK_Orders PRIMARY KEY NONCLUSTERED (CustomerId, OrderNo),
        CONSTRAINT FK_Orders_Customers FOREIGN KEY (CustomerId) REFERENCES dbo.Customers (CustomerId) ON DELETE CASCADE,
        CONSTRAINT CK_Orders_Qty CHECK (Qty > 0)
    )",
    "CREATE CLUSTERED INDEX CIX_Orders_Price ON dbo.Orders (Price)",
    "CREATE TABLE sales.Facts (Id int NOT NULL, Amount decimal(10,2) NOT NULL)",
    "CREATE CLUSTERED COLUMNSTORE INDEX CCI_Facts ON sales.Facts",
    "CREATE TABLE dbo.AllTypes (
        Id int NOT NULL CONSTRAINT PK_AllTypes PRIMARY KEY,
        cBit bit, cTiny tinyint, cSmall smallint, cBig bigint,
        cDec decimal(12,4), cNum numeric(5,0), cMoney money, cSmallMoney smallmoney,
        cReal real, cFloat float,
        cChar char(5), cVarchar varchar(20), cNchar nchar(3), cNvarchar nvarchar(50), cText text, cNtext ntext,
        cBinary binary(3), cVarbinary varbinary(max), cImage image,
        cDate date, cTime time(7), cDatetime datetime, cDatetime2 datetime2(7), cSmallDatetime smalldatetime,
        cDto datetimeoffset(7), cGuid uniqueidentifier, cXml xml, cVariant sql_variant,
        cHier hierarchyid, cGeo geography, cRowver rowversion
    )",
    "CREATE VIEW dbo.vActiveCustomers AS SELECT CustomerId, Email FROM dbo.Customers WHERE Status = 'A'",
    "CREATE PROCEDURE dbo.usp_GetOrders @CustomerId int AS
     BEGIN
         SET NOCOUNT ON;
         SELECT OrderNo, Qty, Price FROM dbo.Orders WHERE CustomerId = @CustomerId;
     END",
    "CREATE FUNCTION dbo.fn_OrderTotal (@CustomerId int) RETURNS decimal(18,2) AS
     BEGIN
         RETURN (SELECT SUM(Total) FROM dbo.Orders WHERE CustomerId = @CustomerId);
     END",
    "CREATE FUNCTION dbo.tvf_CustomerOrders (@CustomerId int) RETURNS TABLE AS
     RETURN SELECT OrderNo, Qty FROM dbo.Orders WHERE CustomerId = @CustomerId",
    "CREATE TRIGGER dbo.trg_Orders_Audit ON dbo.Orders AFTER INSERT AS BEGIN SET NOCOUNT ON; END",
];

const DATA: &[&str] = &[
    "INSERT INTO dbo.AllTypes (Id, cBit, cTiny, cSmall, cBig, cDec, cNum, cMoney, cSmallMoney, cReal, cFloat,
        cChar, cVarchar, cNchar, cNvarchar, cText, cNtext, cBinary, cVarbinary, cImage,
        cDate, cTime, cDatetime, cDatetime2, cSmallDatetime, cDto, cGuid, cXml, cVariant, cHier, cGeo)
     VALUES (1, 1, 255, -32768, 9223372036854775807, 12.5, 12345, 1234.5678, -12.34, 0.1, 3.141592653589793,
        'ab', 'hello', N'é', N'日本語 ✓', 'long text', N'ntext ü', 0x0102, 0xDEADBEEF, 0x00FF,
        '2024-01-02', '10:11:12.1234567', '2024-01-02T10:00:00.123', '2024-01-02T10:00:00.123456',
        '2024-01-02T10:30:00', '2024-01-02T10:00:00.5+05:30', '6F9619FF-8B86-D011-B42D-00C04FC964FF',
        N'<a b=\"1\"/>', CAST(42 AS int), hierarchyid::Parse('/1/2/'), geography::Point(47.65, -122.34, 4326))",
    "INSERT INTO dbo.AllTypes (Id) VALUES (2)",
];

fn table<'a>(s: &'a Schema, schema: &str, name: &str) -> &'a Table {
    s.tables
        .iter()
        .find(|t| t.name.schema == schema && t.name.name == name)
        .unwrap_or_else(|| panic!("table {schema}.{name} missing"))
}

fn column<'a>(t: &'a Table, name: &str) -> &'a Column {
    t.column(name).unwrap_or_else(|| panic!("column {name} missing in {}", t.name))
}

fn check_model(s: &Schema, db: &str) {
    assert_eq!(s.engine, Some(Engine::Mssql));
    assert_eq!(s.database, db);
    assert!(s.server_version.starts_with("Microsoft SQL"), "{}", s.server_version);
    let names: Vec<String> = s.tables.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["dbo.AllTypes", "dbo.Customers", "dbo.Orders", "sales.Facts"]);

    // Customers: identity, collation, defaults, rowversion, PK, unique, filtered index.
    let c = table(s, "dbo", "Customers");
    let id = column(c, "CustomerId");
    assert_eq!((id.data_type.as_str(), id.nullable), ("int", false));
    assert_eq!(id.identity, Some(Identity { seed: 10, increment: 2 }));
    assert_eq!(column(c, "Email").data_type, "nvarchar(200)");
    assert_eq!(column(c, "Email").collation, None, "database default collation is not reported");
    assert_eq!(column(c, "Name").collation.as_deref(), Some("Latin1_General_CS_AS"));
    assert!(column(c, "Name").nullable);
    assert_eq!(column(c, "Status").data_type, "char(1)");
    assert_eq!(column(c, "Status").default.as_deref(), Some("'A'"));
    assert_eq!(column(c, "CreatedAt").data_type, "datetime2(3)");
    assert_eq!(column(c, "CreatedAt").default.as_deref(), Some("sysutcdatetime()"));
    assert_eq!(column(c, "Notes").data_type, "nvarchar(max)");
    assert_eq!(column(c, "RowVer").data_type, "timestamp");
    let col_names: Vec<&str> = c.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(col_names, ["CustomerId", "Email", "Name", "Status", "CreatedAt", "Notes", "RowVer"]);
    assert_eq!(
        c.primary_key,
        Some(KeyConstraint { name: Some("PK_Customers".into()), columns: vec!["CustomerId".into()], clustered: Some(true) })
    );
    assert_eq!(
        c.indexes,
        vec![
            Index {
                name: "IX_Customers_Status".into(),
                columns: vec![
                    IndexColumn { name: "Status".into(), descending: true },
                    IndexColumn { name: "Email".into(), descending: false },
                ],
                unique: false,
                constraint: false,
                method: None,
                filter: Some("([Status]<>'X')".into()),
                include: vec!["Name".into()],
            },
            Index {
                name: "UQ_Customers_Email".into(),
                columns: vec![IndexColumn { name: "Email".into(), descending: false }],
                unique: true,
                constraint: true,
                method: None,
                filter: None,
                include: vec![],
            },
        ]
    );

    // Orders: composite nonclustered PK, computed columns, FK cascade, check, clustered index.
    let o = table(s, "dbo", "Orders");
    assert_eq!(
        o.primary_key,
        Some(KeyConstraint {
            name: Some("PK_Orders".into()),
            columns: vec!["CustomerId".into(), "OrderNo".into()],
            clustered: Some(false)
        })
    );
    assert_eq!(column(o, "Qty").default.as_deref(), Some("1"));
    assert_eq!(column(o, "Price").data_type, "decimal(10,2)");
    assert_eq!(column(o, "Total").computed.as_deref(), Some("[Qty]*[Price] PERSISTED"));
    assert_eq!(column(o, "Discount").computed.as_deref(), Some("[Price]/(10)"));
    assert_eq!(
        o.foreign_keys,
        vec![ForeignKey {
            name: "FK_Orders_Customers".into(),
            columns: vec!["CustomerId".into()],
            ref_table: ObjectName::new("dbo", "Customers"),
            ref_columns: vec!["CustomerId".into()],
            on_delete: Some("CASCADE".into()),
            on_update: None,
        }]
    );
    assert_eq!(o.checks, vec![CheckConstraint { name: "CK_Orders_Qty".into(), expression: "[Qty]>(0)".into() }]);
    assert_eq!(o.indexes.len(), 1);
    assert_eq!(o.indexes[0].name, "CIX_Orders_Price");
    assert_eq!(o.indexes[0].method.as_deref(), Some("clustered"));
    assert_eq!(o.indexes[0].columns, vec![IndexColumn { name: "Price".into(), descending: false }]);

    let f = table(s, "sales", "Facts");
    assert_eq!(f.primary_key, None);
    assert_eq!(f.indexes.len(), 1);
    assert_eq!(f.indexes[0].method.as_deref(), Some("clustered columnstore"));
    assert!(f.indexes[0].columns.is_empty());

    let t = table(s, "dbo", "AllTypes");
    for (name, ty) in [
        ("cNchar", "nchar(3)"),
        ("cBinary", "binary(3)"),
        ("cVarbinary", "varbinary(max)"),
        ("cDec", "decimal(12,4)"),
        ("cNum", "numeric(5,0)"),
        ("cTime", "time(7)"),
        ("cDatetime2", "datetime2(7)"),
        ("cDto", "datetimeoffset(7)"),
        ("cFloat", "float"),
        ("cReal", "real"),
        ("cMoney", "money"),
        ("cXml", "xml"),
        ("cHier", "hierarchyid"),
        ("cGeo", "geography"),
        ("cVariant", "sql_variant"),
    ] {
        assert_eq!(column(t, name).data_type, ty, "{name}");
    }

    // Code objects, sorted by (schema, name).
    let objs: Vec<(ObjectKind, String)> = s.objects.iter().map(|o| (o.kind, o.name.to_string())).collect();
    assert_eq!(
        objs,
        [
            // Byte order: upper-case names sort first.
            (ObjectKind::Sequence, "dbo.OrderNumbers".to_owned()),
            (ObjectKind::Function, "dbo.fn_OrderTotal".to_owned()),
            (ObjectKind::Trigger, "dbo.trg_Orders_Audit".to_owned()),
            (ObjectKind::Function, "dbo.tvf_CustomerOrders".to_owned()),
            (ObjectKind::Procedure, "dbo.usp_GetOrders".to_owned()),
            (ObjectKind::View, "dbo.vActiveCustomers".to_owned()),
        ]
    );
    for o in &s.objects {
        assert!(o.definition.starts_with("CREATE"), "{}: {}", o.name, o.definition);
        assert!(!o.definition.ends_with(';'));
    }
    let seq = s.objects.iter().find(|o| o.kind == ObjectKind::Sequence).unwrap();
    assert_eq!(seq.definition, "CREATE SEQUENCE [dbo].[OrderNumbers] AS bigint START WITH 1000 INCREMENT BY 5");
    let trg = s.objects.iter().find(|o| o.kind == ObjectKind::Trigger).unwrap();
    assert_eq!(trg.table, Some(ObjectName::new("dbo", "Orders")));
}

async fn check_schema_filter(cfg: &ConnectionConfig) {
    let cfg = ConnectionConfig { schemas: vec!["sales".into()], ..cfg.clone() };
    let s = connect(&cfg).await.load_schema().await.unwrap();
    let names: Vec<String> = s.tables.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["sales.Facts"]);
    assert!(s.objects.is_empty());
}

const ALL_TYPE_COLUMNS: &[&str] = &[
    "Id", "cBit", "cTiny", "cSmall", "cBig", "cDec", "cNum", "cMoney", "cSmallMoney", "cReal", "cFloat", "cChar",
    "cVarchar", "cNchar", "cNvarchar", "cText", "cNtext", "cBinary", "cVarbinary", "cImage", "cDate", "cTime",
    "cDatetime", "cDatetime2", "cSmallDatetime", "cDto", "cGuid", "cXml", "cVariant", "cHier", "cGeo", "cRowver",
];

fn all_type_columns() -> Vec<String> {
    ALL_TYPE_COLUMNS.iter().map(|s| s.to_string()).collect()
}

async fn check_rows(a: &mut Client) {
    use Value::*;
    let t = ObjectName::new("dbo", "AllTypes");
    let rows = a.fetch_rows(&t, &all_type_columns(), &["Id".into()]).await.unwrap();
    assert_eq!(rows.len(), 2);
    let r = &rows[0];
    let expected = [
        Int(1),
        Bool(true),
        Int(255),
        Int(-32768),
        Int(i64::MAX),
        Decimal("12.5".into()),
        Decimal("12345".into()),
        Decimal("1234.5678".into()),
        Decimal("-12.34".into()),
        Float(0.1),
        Float(std::f64::consts::PI),
        Text("ab   ".into()),
        Text("hello".into()),
        Text("é  ".into()),
        Text("日本語 ✓".into()),
        Text("long text".into()),
        Text("ntext ü".into()),
        Bytes(vec![1, 2, 0]),
        Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        Bytes(vec![0x00, 0xff]),
        Date("2024-01-02".into()),
        Time("10:11:12.1234567".into()),
        DateTime("2024-01-02 10:00:00.123".into()),
        DateTime("2024-01-02 10:00:00.123456".into()),
        DateTime("2024-01-02 10:30:00".into()),
        DateTimeTz("2024-01-02 10:00:00.5+05:30".into()),
        Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".into()),
    ];
    for (i, want) in expected.iter().enumerate() {
        assert_eq!(&r[i], want, "column {}", ALL_TYPE_COLUMNS[i]);
    }
    match &r[27] {
        Document(x) => assert!(x.starts_with("<a") && x.contains("b=\"1\""), "{x}"),
        v => panic!("xml: {v:?}"),
    }
    assert_eq!(r[28], Text("42".into()));
    assert_eq!(r[29], Text("/1/2/".into()));
    assert_eq!(r[30], Text("POINT (-122.34 47.65)".into()));
    assert!(matches!(&r[31], Bytes(b) if b.len() == 8), "rowversion: {:?}", r[31]);

    let nulls = &rows[1];
    assert_eq!(nulls[0], Int(2));
    assert!(nulls[1..31].iter().all(Value::is_null), "{nulls:?}");
}

async fn check_execute(a: &mut Client) {
    let d = dialect(Engine::Mssql);
    let customers = ObjectName::new("dbo", "Customers");
    let emails = |rows: Vec<Vec<Value>>| rows.into_iter().map(|r| r[0].to_string()).collect::<Vec<_>>();

    // A failing second statement rolls back the first.
    let stmts = vec![
        "INSERT INTO dbo.Customers (Email) VALUES (N'rollback@example.com')".to_owned(),
        "INSERT INTO dbo.Customers (Email) VALUES (N'rollback@example.com')".to_owned(),
    ];
    let err = a.execute(&stmts, true).await.expect_err("duplicate email must fail").to_string();
    assert!(err.contains("statement 2"), "{err}");
    let rows = a.fetch_rows(&customers, &["Email".into()], &["Email".into()]).await.unwrap();
    assert!(!emails(rows).contains(&"rollback@example.com".to_owned()), "transaction was not rolled back");

    // SET IDENTITY_INSERT must survive across batches (no sp_executesql).
    let cols = vec!["CustomerId".to_owned(), "Email".to_owned()];
    let stmts = vec![
        d.identity_insert(&customers, true).unwrap(),
        d.insert(&customers, &cols, &[Value::Int(500), Value::Text("ident@example.com".into())]),
        d.identity_insert(&customers, false).unwrap(),
    ];
    a.execute(&stmts, true).await.unwrap();
    let rows = a.fetch_rows(&customers, &cols, &["CustomerId".into()]).await.unwrap();
    assert_eq!(rows, vec![vec![Value::Int(500), Value::Text("ident@example.com".into())]]);

    // FK toggles run cleanly.
    let orders = [ObjectName::new("dbo", "Orders")];
    let mut toggles = d.disable_foreign_keys(&orders);
    toggles.extend(d.enable_foreign_keys(&orders));
    a.execute(&toggles, false).await.unwrap();
}

/// Replays A's model into B through the dialect; B must introspect identically.
async fn round_trip(a: &mut Client, schema_a: &Schema, b: &mut Client) {
    let d = dialect(Engine::Mssql);
    let mut stmts = vec!["CREATE SCHEMA sales".to_owned()];
    stmts.extend(schema_a.tables.iter().map(|t| d.create_table(t)));
    for t in &schema_a.tables {
        stmts.extend(t.indexes.iter().filter(|i| !i.constraint).map(|i| d.create_index(&t.name, i)));
    }
    for t in &schema_a.tables {
        stmts.extend(t.foreign_keys.iter().map(|fk| d.add_foreign_key(&t.name, fk)));
    }
    let order = [ObjectKind::Sequence, ObjectKind::Function, ObjectKind::View, ObjectKind::Procedure, ObjectKind::Trigger];
    for kind in order {
        for o in schema_a.objects.iter().filter(|o| o.kind == kind) {
            stmts.extend(d.create_object(o));
        }
    }
    b.execute(&stmts, true).await.unwrap_or_else(|e| panic!("{e}"));

    let schema_b = b.load_schema().await.unwrap();
    assert_eq!(schema_b.tables, schema_a.tables);
    assert_eq!(schema_b.objects, schema_a.objects);

    // Data round trip through `literal` (rowversion can't be inserted).
    let t = ObjectName::new("dbo", "AllTypes");
    let cols = all_type_columns();
    let insertable = &cols[..cols.len() - 1];
    let rows_a = a.fetch_rows(&t, insertable, &["Id".into()]).await.unwrap();
    let inserts: Vec<String> = rows_a.iter().map(|r| d.insert(&t, insertable, r)).collect();
    b.execute(&inserts, true).await.unwrap_or_else(|e| panic!("{e}"));
    let rows_b = b.fetch_rows(&t, insertable, &["Id".into()]).await.unwrap();
    assert_eq!(rows_b, rows_a);
}

async fn check_alters(b: &mut Client) {
    let d = dialect(Engine::Mssql);
    let customers = ObjectName::new("dbo", "Customers");
    let load = |s: &Schema| table(s, "dbo", "Customers").clone();

    let before = load(&b.load_schema().await.unwrap());
    let notes = column(&before, "Notes").clone();

    // Type + nullability + new default.
    let v500 = Column {
        data_type: "nvarchar(500)".into(),
        nullable: false,
        default: Some("N'none'".into()),
        ..notes.clone()
    };
    b.execute(&d.alter_column(&customers, &notes, &v500), true).await.unwrap_or_else(|e| panic!("{e}"));
    let after = load(&b.load_schema().await.unwrap());
    assert_eq!(column(&after, "Notes"), &v500);

    // Type change with an existing default: drop, alter, re-add.
    let v600 = Column { data_type: "nvarchar(600)".into(), ..v500.clone() };
    b.execute(&d.alter_column(&customers, &v500, &v600), true).await.unwrap_or_else(|e| panic!("{e}"));
    let after = load(&b.load_schema().await.unwrap());
    assert_eq!(column(&after, "Notes"), &v600);

    // Dropping a column with a (system-agnostic) default constraint.
    b.execute(&[d.drop_column(&customers, "CreatedAt")], true).await.unwrap_or_else(|e| panic!("{e}"));
    let after = load(&b.load_schema().await.unwrap());
    assert!(after.column("CreatedAt").is_none());

    // CREATE OR ALTER keeps the object; the stored text is normalised back to CREATE.
    let schema = b.load_schema().await.unwrap();
    let old = schema.objects.iter().find(|o| o.kind == ObjectKind::View).unwrap().clone();
    let new = CodeObject {
        definition: "CREATE VIEW dbo.vActiveCustomers AS SELECT CustomerId FROM dbo.Customers".into(),
        ..old.clone()
    };
    b.execute(&d.replace_object(&old, &new), true).await.unwrap_or_else(|e| panic!("{e}"));
    let schema = b.load_schema().await.unwrap();
    let view = schema.objects.iter().find(|o| o.kind == ObjectKind::View).unwrap();
    assert_eq!(view.definition, new.definition);

    // Sequence replace = drop + create.
    let seq = schema.objects.iter().find(|o| o.kind == ObjectKind::Sequence).unwrap().clone();
    let seq2 = CodeObject {
        definition: "CREATE SEQUENCE [dbo].[OrderNumbers] AS int START WITH 1 INCREMENT BY 1".into(),
        ..seq.clone()
    };
    b.execute(&d.replace_object(&seq, &seq2), true).await.unwrap_or_else(|e| panic!("{e}"));

    // Rename.
    b.execute(&[d.rename_table(&ObjectName::new("sales", "Facts"), "Facts2")], false)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let schema = b.load_schema().await.unwrap();
    assert!(schema.tables.iter().any(|t| t.name == ObjectName::new("sales", "Facts2")));
    let seq = schema.objects.iter().find(|o| o.kind == ObjectKind::Sequence).unwrap();
    assert_eq!(seq.definition, seq2.definition);
}
