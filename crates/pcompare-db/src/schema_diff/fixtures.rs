//! Hand-built schema pieces shared by the diff and script tests.

use crate::model::*;

pub fn col(name: &str, ty: &str) -> Column {
    Column { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
}

pub fn not_null(mut c: Column) -> Column {
    c.nullable = false;
    c
}

pub fn identity(mut c: Column) -> Column {
    c.identity = Some(Identity::default());
    c.nullable = false;
    c
}

pub fn with_default(mut c: Column, d: &str) -> Column {
    c.default = Some(d.into());
    c
}

pub fn table(name: &str, columns: Vec<Column>) -> Table {
    Table { name: ObjectName::new("dbo", name), columns, ..Default::default() }
}

pub fn with_pk(mut t: Table, name: &str, cols: &[&str]) -> Table {
    t.primary_key =
        Some(KeyConstraint { name: Some(name.into()), columns: cols.iter().map(|c| c.to_string()).collect(), clustered: None });
    t
}

pub fn index(name: &str, cols: &[&str], unique: bool) -> Index {
    Index {
        name: name.into(),
        columns: cols.iter().map(|c| IndexColumn { name: c.to_string(), descending: false }).collect(),
        unique,
        ..Default::default()
    }
}

pub fn fk(name: &str, cols: &[&str], ref_table: &str, ref_cols: &[&str]) -> ForeignKey {
    ForeignKey {
        name: name.into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        ref_table: ObjectName::new("dbo", ref_table),
        ref_columns: ref_cols.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    }
}

pub fn check(name: &str, expr: &str) -> CheckConstraint {
    CheckConstraint { name: name.into(), expression: expr.into() }
}

pub fn code(kind: ObjectKind, name: &str, definition: &str) -> CodeObject {
    CodeObject { kind, name: ObjectName::new("dbo", name), definition: definition.into(), table: None }
}

pub fn trigger(name: &str, on: &str, definition: &str) -> CodeObject {
    CodeObject { table: Some(ObjectName::new("dbo", on)), ..code(ObjectKind::Trigger, name, definition) }
}

pub fn schema(tables: Vec<Table>, objects: Vec<CodeObject>) -> Schema {
    Schema { tables, objects, ..Default::default() }
}

/// Customers(Id PK, Email) ← Orders(Id PK, CustomerId FK, Total).
pub fn shop() -> Schema {
    let mut customers = with_pk(
        table("Customers", vec![identity(col("Id", "int")), col("Email", "nvarchar(50)")]),
        "PK_Customers",
        &["Id"],
    );
    customers.indexes.push(index("IX_Customers_Email", &["Email"], true));
    let mut orders = with_pk(
        table("Orders", vec![not_null(col("Id", "int")), not_null(col("CustomerId", "int")), with_default(col("Total", "decimal(10,2)"), "((0))")]),
        "PK_Orders",
        &["Id"],
    );
    orders.foreign_keys.push(fk("FK_Order_Customer", &["CustomerId"], "Customers", &["Id"]));
    orders.checks.push(check("CK_Orders_Total", "([Total]>=(0))"));
    schema(
        vec![customers, orders],
        vec![code(ObjectKind::View, "BigOrders", "CREATE VIEW dbo.BigOrders AS SELECT * FROM dbo.Orders WHERE Total > 100")],
    )
}
