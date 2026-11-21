//! End-to-end remote client coverage: a real `WireServe` listener plus the
//! product `Client`, exercising table creation, prepared exact values, cross
//! table transactions, transport reconnect and terminal close.

use std::collections::BTreeMap;

use database_client::yy::{Client, ClientState, Error, Row};
use database_execution::engine::{Connection, Value};
use database_server::yydb::{AuthBootstrap, ServeOptions, WireServe, connection_factory};
use database_types::{Decimal, Timestamp};

const SCHEMA: &str = "\
table Item {
    @@id: uuid,
    name: utf8,
    qty: i64,
    price: Decimal<12, 2>,
    at: datetime,
}

table Audit {
    @@id: uuid,
    item: uuid,
    note: utf8,
    at: datetime,
}
";

struct Node {
    serve: Option<WireServe>,
    endpoint: String,
}

impl Node {
    fn start() -> Self {
        let connection = Connection::open_in_memory().expect("open in-memory engine");
        connection.ensure_schema(SCHEMA).expect("install schema");
        let options = ServeOptions {
            bind: "127.0.0.1:0".parse().expect("bind address"),
            bootstrap: AuthBootstrap { tokens: Vec::new() },
            anonymous_loopback: true,
            label: "yydb-remote-client-test",
        };
        let serve = WireServe::start(options, connection_factory(connection)).expect("start wire server");
        Self { endpoint: serve.address().to_string(), serve: Some(serve) }
    }

    fn client(&self) -> Client {
        Client::connect(self.endpoint.clone()).expect("connect client")
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(serve) = self.serve.take() {
            let _ = serve.stop();
        }
    }
}

fn item(id: uuid::Uuid, name: &str, qty: i64, price: &str) -> BTreeMap<String, Value> {
    // VOS write coercion admits exact scalars as their canonical text literal;
    // the stored and read-back values are the typed Date/Time/Decimal forms.
    BTreeMap::from([
        ("id".to_owned(), Value::Uuid(id)),
        ("name".to_owned(), Value::Text(name.to_owned())),
        ("qty".to_owned(), Value::I64(qty)),
        ("price".to_owned(), Value::Text(price.to_owned())),
        ("at".to_owned(), Value::Text("2024-02-29T12:34:56.123456Z".to_owned())),
    ])
}

fn audit(id: uuid::Uuid, item: uuid::Uuid, note: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("id".to_owned(), Value::Uuid(id)),
        ("item".to_owned(), Value::Uuid(item)),
        ("note".to_owned(), Value::Text(note.to_owned())),
        ("at".to_owned(), Value::Text("2024-03-01T00:00:00Z".to_owned())),
    ])
}

fn item_ids(rows: &[Row]) -> Vec<uuid::Uuid> {
    rows.iter()
        .map(|row| match row.get("id") {
            Some(Value::Uuid(id)) => *id,
            other => panic!("row id is not a uuid: {other:?}"),
        })
        .collect()
}

#[test]
fn table_creation_and_projection_keep_prepared_exact_values() {
    let node = Node::start();
    let client = node.client();
    client.ensure_schema(SCHEMA).expect("re-ensure schema through the wire");

    let table = client.table("Item").expect("table projection");
    let id = uuid::Uuid::now_v7();
    table.insert(&item(id, "widget", 7, "1234.50")).expect("insert through table projection");

    let rows = table.filter_eq("id", Value::Uuid(id)).expect("filter by primary key");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], Value::Text("widget".to_owned()));
    assert_eq!(rows[0]["qty"], Value::I64(7));
    assert_eq!(rows[0]["price"], Value::Decimal(Decimal::parse("1234.50", 12, 2).expect("decimal")));
    assert_eq!(rows[0]["at"], Value::Timestamp(Timestamp::parse_iso("2024-02-29T12:34:56.123456Z").expect("timestamp")));

    assert_eq!(table.take(1).expect("take").len(), 1);
    assert_eq!(table.all().expect("all").len(), 1);
}

#[test]
fn prepared_statement_returns_the_exact_bound_values() {
    let node = Node::start();
    let client = node.client();
    let id = uuid::Uuid::now_v7();
    let params = item(id, "prepared", 3, "0.01");

    let statement = client.prepare("Item.insert({ id: $id, name: $name, qty: $qty, price: $price, at: $at })").expect("prepare insert");
    client.execute_prepared(&statement, &params).expect("execute insert");

    let read = client.prepare("Item.all().filter(i => i.id == $id).collect()").expect("prepare read");
    let rows = client.execute_prepared(&read, &BTreeMap::from([("id".to_owned(), Value::Uuid(id))])).expect("execute read");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], Value::Uuid(id), "the uuid parameter survives the wire");
    assert_eq!(rows[0]["name"], Value::Text("prepared".to_owned()));
    assert_eq!(rows[0]["qty"], Value::I64(3));
    assert_eq!(rows[0]["price"], Value::Decimal(Decimal::parse("0.01", 12, 2).expect("decimal")), "the decimal literal reads back exact");
    assert_eq!(
        rows[0]["at"],
        Value::Timestamp(Timestamp::parse_iso("2024-02-29T12:34:56.123456Z").expect("timestamp")),
        "the datetime literal reads back exact"
    );
}

#[test]
fn cross_table_transactions_commit_and_roll_back() {
    let node = Node::start();
    let client = node.client();
    let items = client.table("Item").expect("item table");
    let audits = client.table("Audit").expect("audit table");
    let item_id = uuid::Uuid::now_v7();

    client.begin().expect("begin");
    items.insert(&item(item_id, "rolled-back", 1, "1.00")).expect("insert item");
    audits.insert(&audit(uuid::Uuid::now_v7(), item_id, "rolled-back")).expect("insert audit");
    client.rollback().expect("rollback");
    assert!(items.all().expect("items").is_empty(), "a rolled back insert must leave no item");
    assert!(audits.all().expect("audits").is_empty(), "a rolled back insert must leave no audit row");

    client.begin().expect("begin");
    let committed = uuid::Uuid::now_v7();
    items.insert(&item(committed, "committed", 2, "2.00")).expect("insert item");
    audits.insert(&audit(uuid::Uuid::now_v7(), committed, "committed")).expect("insert audit");
    client.commit().expect("commit");

    assert_eq!(item_ids(&items.all().expect("items")), vec![committed]);
    assert_eq!(audits.all().expect("audits").len(), 1, "the audit row commits with the item row");
}

#[test]
fn transport_reconnects_and_close_is_terminal() {
    let node = Node::start();
    let client = node.client();
    assert_eq!(client.state(), ClientState::Connected);
    assert!(client.query("Item.all().collect()", &BTreeMap::new()).expect("initial query").is_empty());

    client.disconnect().expect("drop transport");
    assert_eq!(client.state(), ClientState::Disconnected);
    assert!(matches!(client.prepare("Item.all().collect()"), Err(Error::Protocol { .. })), "a disconnected client must not send frames");

    client.reconnect().expect("reconnect");
    assert_eq!(client.state(), ClientState::Connected);
    assert!(client.query("Item.all().collect()", &BTreeMap::new()).expect("query after reconnect").is_empty());

    client.close().expect("close");
    assert_eq!(client.state(), ClientState::Closed);
    client.close().expect("repeated close succeeds");
    assert!(matches!(client.begin(), Err(Error::SessionClosed)), "a closed session is rejected");
    assert!(matches!(client.reconnect(), Err(Error::SessionClosed)), "closed is terminal");
}

#[test]
fn websocket_transport_speaks_the_same_frames() {
    let node = Node::start();
    let client = Client::connect(format!("ws://{}", node.endpoint)).expect("connect over websocket");
    let table = client.table("Item").expect("table projection");
    let id = uuid::Uuid::now_v7();
    table.insert(&item(id, "ws", 9, "9.99")).expect("insert over websocket");
    let rows = table.all().expect("read over websocket");
    assert_eq!(item_ids(&rows), vec![id]);
    client.close().expect("close websocket session");
}
