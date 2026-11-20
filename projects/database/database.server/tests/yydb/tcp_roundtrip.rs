use std::{
    net::TcpListener,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use database_client::yy::{Client, Error};
use database_execution::engine::{Connection, Value};
use database_protocol_yy::{
    decode_prepare_ok, encode_execute_prepared, encode_prepare, read_frame, write_frame, Frame, MessageType,
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
