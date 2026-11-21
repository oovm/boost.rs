use std::{
    collections::BTreeMap,
    net::TcpListener,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use database_client::yy::{Client, Error};
use database_execution::engine::{Connection, Value};
use database_protocol_yy::{
    ExecutePreparedRows, decode_execute_prepared_ok, decode_prepare_ok, encode_execute_prepared, encode_prepare, read_frame, write_frame, Frame,
    MessageType,
};
use database_server::yydb::{SessionState, dispatch};

#[test]
fn connect_rejects_empty_endpoint() {
    assert!(matches!(Client::connect("  "), Err(Error::Unsupported(_))));
}

#[test]
fn remote_error_keeps_stable_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let conn = Arc::new(Connection::open_in_memory().unwrap());

    let server_conn = Arc::clone(&conn);
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let state = SessionState::for_test(1);
        // Mock server — pre-authenticated via SessionState::for_test
        while let Ok(request) = read_frame(&mut stream) {
            let response = dispatch(server_conn.as_ref(), &state, &request);
            write_frame(&mut stream, &response).unwrap();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let client = loop {
        match Client::connect(addr.to_string()) {
            Ok(client) => break client,
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("{error}"),
        }
    };

    match client.ensure_schema("table {") {
        Err(Error::Remote { code, message }) => {
            assert_eq!(code, "database.schema.invalid");
            assert!(!message.is_empty());
        }
        other => panic!("expected remote structured error, got {other:?}"),
    }
}

#[test]
fn prepared_vos_write_through_dispatch_actually_writes() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Note { @@id: i64, body: utf8 }").unwrap();
    let state = SessionState::for_test(7);

    let prepare = Frame::new(MessageType::Prepare, 1, encode_prepare(r#"Note::insert({ id: 1, body: "hi" })"#));
    let prepared = dispatch(&conn, &state, &prepare);
    assert!(matches!(prepared.msg_type, MessageType::PrepareOk), "prepare failed: {:?}", prepared.msg_type);
    let statement_id = decode_prepare_ok(&prepared.body).unwrap();

    let execute = Frame::new(MessageType::ExecutePrepared, 2, encode_execute_prepared(&statement_id, None));
    let executed = dispatch(&conn, &state, &execute);
    assert!(matches!(executed.msg_type, MessageType::ExecutePreparedOk), "execute failed: {:?}", executed.msg_type);

    let rows = conn.query_vos("Note.first(x => x.id == 1)").unwrap();
    assert_eq!(rows.len(), 1, "a prepared VOS insert must persist its row");
}

#[test]
fn tcp_roundtrip_against_local_dispatch_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let conn = Arc::new(Connection::open_in_memory().unwrap());
    conn.ensure_schema("table T { @@id: uuid }").unwrap();

    let server_conn = Arc::clone(&conn);
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let state = SessionState::for_test(2);
        // Mock server — pre-authenticated via SessionState::for_test
        while let Ok(request) = read_frame(&mut stream) {
            let response = dispatch(server_conn.as_ref(), &state, &request);
            write_frame(&mut stream, &response).unwrap();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let client = loop {
        match Client::connect(addr.to_string()) {
            Ok(client) => break client,
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("{error}"),
        }
    };

    client.put("k", b"v").unwrap();
    assert_eq!(client.get("k").unwrap().as_deref(), Some(b"v".as_slice()));
    let schema = client.schema().unwrap().unwrap();
    assert_eq!(schema.version, 1);

    conn.create_scalar("double", 1, |args| match args {
        [Value::I64(n)] => Ok(Value::I64(n * 2)),
        _ => Err(Error::Udf { name: "double".into(), message: "expected i64".into() }),
    })
    .unwrap();
    assert_eq!(client.call_scalar_version("double", 1, &[Value::I64(21)]).unwrap(), Value::I64(42));
}

/// Named types (class + structure + unite) take the independent wire projection:
/// the server encodes from its runtime directory and the remote client decodes
/// the same values back, with no storage tag or client-side schema contract.
#[test]
fn prepared_read_round_trips_class_structure_and_unite_through_the_wire_client() {
    const SCHEMA: &str = "\
class Profile { name: utf8, age: i64, bio: utf8? }

structure Address { zip: utf8, country: utf8? }

unite Contact {
    Email { address: utf8, label: utf8? },
    Sms { number: utf8 },
    None,
}

table Person {
    @@id: i64,
    profile: Profile,
    address: Address,
    contact: Contact?,
}
";

    let conn = Arc::new(Connection::open_in_memory().unwrap());
    conn.ensure_schema(SCHEMA).unwrap();
    let params = BTreeMap::from([
        (
            "profile".to_owned(),
            Value::Row(BTreeMap::from([
                ("name".to_owned(), Value::Text("Ada".to_owned())),
                ("age".to_owned(), Value::I64(36)),
            ])),
        ),
        (
            "address".to_owned(),
            Value::Row(BTreeMap::from([
                ("zip".to_owned(), Value::Text("100000".to_owned())),
                ("country".to_owned(), Value::Text("CN".to_owned())),
            ])),
        ),
        (
            "contact".to_owned(),
            Value::Row(BTreeMap::from([(
                "Email".to_owned(),
                Value::Row(BTreeMap::from([("address".to_owned(), Value::Text("ada@example.com".to_owned()))])),
            )])),
        ),
    ]);
    let named: BTreeMap<String, Value> =
        std::iter::once(("id".to_owned(), Value::I64(1))).chain(params).collect();
    conn.execute_vos_write_with_params(
        "Person.insert({ id: $id, profile: $profile, address: $address, contact: $contact })",
        &named,
    )
    .unwrap();

    // The server must choose the named projection from its runtime directory,
    // not from the shape of the result values.
    let state = SessionState::for_test(9);
    let prepare = Frame::new(MessageType::Prepare, 1, encode_prepare("Person.all().collect()"));
    let prepared = dispatch(conn.as_ref(), &state, &prepare);
    assert_eq!(prepared.msg_type, MessageType::PrepareOk);
    let statement_id = decode_prepare_ok(&prepared.body).unwrap();
    let execute = Frame::new(MessageType::ExecutePrepared, 2, encode_execute_prepared(&statement_id, None));
    let executed = dispatch(conn.as_ref(), &state, &execute);
    assert_eq!(executed.msg_type, MessageType::ExecutePreparedOk);
    assert!(
        matches!(decode_execute_prepared_ok(&executed.body).expect("decode result frame"), ExecutePreparedRows::Named { .. }),
        "a schema declaring named types must use the named wire projection"
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_conn = Arc::clone(&conn);
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let state = SessionState::for_test(3);
        while let Ok(request) = read_frame(&mut stream) {
            let response = dispatch(server_conn.as_ref(), &state, &request);
            write_frame(&mut stream, &response).unwrap();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let client = loop {
        match Client::connect(addr.to_string()) {
            Ok(client) => break client,
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("{error}"),
        }
    };

    let rows = client.query("Person.all().collect()", &BTreeMap::new()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("id"), Some(&Value::I64(1)));
    assert_eq!(
        rows[0].get("profile"),
        Some(&Value::Row(BTreeMap::from([
            ("name".to_owned(), Value::Text("Ada".to_owned())),
            ("age".to_owned(), Value::I64(36)),
            ("bio".to_owned(), Value::Null),
        ]))),
        "a class value keeps its declared members across the wire"
    );
    assert_eq!(
        rows[0].get("address"),
        Some(&Value::Row(BTreeMap::from([
            ("zip".to_owned(), Value::Text("100000".to_owned())),
            ("country".to_owned(), Value::Text("CN".to_owned())),
        ]))),
        "a structure value decodes from its closed wire layout"
    );
    assert_eq!(
        rows[0].get("contact"),
        Some(&Value::List(vec![
            Value::I64(0),
            Value::Row(BTreeMap::from([
                ("address".to_owned(), Value::Text("ada@example.com".to_owned())),
                ("label".to_owned(), Value::Null),
            ])),
        ])),
        "a unite value keeps its discriminant and payload across the wire"
    );
}
