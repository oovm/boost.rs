//! 把 YYDS 的分片 KV 执行器接入共享 YY 线服务。
//!
//! 线服务的 TCP/WebSocket 循环、鉴权门与权限门统一由 `database.server::yydb`
//! 提供;本模块只实现执行后端(分片 `KeyValueExecutor` 与可选 catalog 的正式
//! schema 变更服务 `CatalogSchemaService`)以及 YYDS 自己的 bootstrap 配置读取。

use std::{net::SocketAddr, sync::Arc};

use database::{
    execution::cluster::{CatalogSchemaService, KeyValueExecutor},
    query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult},
    server::yydb::{AuthBootstrap, BackendFactory, Principal, PrincipalKind, Role, ServeBackend, ServeOptions},
    types::{Error as DatabaseError, Result as DatabaseResult, SchemaVersion, cluster::Namespace},
};

/// YY 原生线协议没有命名空间概念,统一落在固定命名空间。
pub const DEFAULT_NAMESPACE: &str = "default";

/// 分片执行器后端:一条连接一个实例,共享同一个 `KeyValueExecutor` 与 catalog。
pub struct KvBackend {
    executor: Arc<dyn KeyValueExecutor>,
    catalog: Option<CatalogSchemaService>,
}

impl ServeBackend for KvBackend {
    fn info(&self) -> DatabaseResult<Vec<u8>> {
        let mut text = format!("product=yyds\n{}\n", env!("CARGO_PKG_VERSION"));
        if let Some(catalog) = &self.catalog {
            let schema_line = match catalog.schema() {
                Some(schema) => format!("schema.version={}\n", schema.version),
                None => "schema.version=\n".to_owned(),
            };
            text.push_str(&schema_line);
        }
        Ok(text.into_bytes())
    }

    fn schema(&self) -> DatabaseResult<Option<SchemaVersion>> {
        let Some(catalog) = &self.catalog
        else {
            return Ok(None);
        };
        Ok(catalog.schema().map(|schema| SchemaVersion { version: schema.version, document: schema.document }))
    }

    fn ensure_schema(&self, version: u32, document: &str) -> DatabaseResult<()> {
        let Some(catalog) = &self.catalog
        else {
            return Err(DatabaseError::Unsupported("YYDS wire catalog is not wired up yet"));
        };
        catalog.ensure_schema(version, document)?;
        Ok(())
    }

    fn kv_get(&self, key: &str) -> DatabaseResult<Option<Vec<u8>>> {
        let command =
            KeyValueCommand { namespace: Namespace(DEFAULT_NAMESPACE.to_string()), key: key.as_bytes().to_vec(), action: KeyValueAction::Get };
        match self.executor.execute(None, command) {
            Ok(KeyValueResult::Get(value)) => Ok(value),
            Ok(_) => Err(DatabaseError::Protocol { message: "executor returned unexpected result for Get".to_owned() }),
            Err(error) => Err(DatabaseError::from(error)),
        }
    }

    fn kv_put(&self, key: String, value: Vec<u8>) -> DatabaseResult<()> {
        let command =
            KeyValueCommand { namespace: Namespace(DEFAULT_NAMESPACE.to_string()), key: key.into_bytes(), action: KeyValueAction::Put(value) };
        match self.executor.execute(None, command) {
            Ok(KeyValueResult::Put { .. }) => Ok(()),
            Ok(_) => Err(DatabaseError::Protocol { message: "executor returned unexpected result for Put".to_owned() }),
            Err(error) => Err(DatabaseError::from(error)),
        }
    }
}

/// 为每条连接打开 [`KvBackend`]。
pub struct KvBackendFactory {
    executor: Arc<dyn KeyValueExecutor>,
    catalog: Option<CatalogSchemaService>,
}

impl KvBackendFactory {
    /// 用分片执行器与可选 catalog schema 服务建立工厂。
    pub fn new(executor: Arc<dyn KeyValueExecutor>, catalog: Option<CatalogSchemaService>) -> Self {
        Self { executor, catalog }
    }
}

impl BackendFactory for KvBackendFactory {
    fn open(&self, _peer_is_loopback: bool) -> Box<dyn ServeBackend> {
        Box::new(KvBackend { executor: Arc::clone(&self.executor), catalog: self.catalog.clone() })
    }
}

/// 从 `YYDS_AUTH_TOKENS` 构造线服务参数。
///
/// 分号分隔的每个 token 成为一个 `SchemaAdmin` 服务主体;回环绑定允许
/// 匿名开发模式(连接进入即预认证),非回环绑定必须显式配置 token。
pub fn serve_options(address: SocketAddr) -> Result<ServeOptions, String> {
    let raw = std::env::var("YYDS_AUTH_TOKENS").ok();
    let tokens: Vec<String> =
        raw.as_deref().unwrap_or_default().split(';').map(str::trim).filter(|token| !token.is_empty()).map(str::to_owned).collect();
    if raw.is_some() && tokens.is_empty() {
        return Err("YYDS_AUTH_TOKENS parsed to no entries — set at least one token".to_owned());
    }
    let loopback = address.ip().is_loopback();
    if tokens.is_empty() && !loopback {
        return Err("non-loopback bind requires explicit bootstrap token(s) — \
                    set YYDS_AUTH_TOKENS (semicolon-separated) before starting the wire server"
            .to_owned());
    }
    let bootstrap = AuthBootstrap {
        tokens: tokens
            .into_iter()
            .map(|token| (token, Principal { name: "wire".to_owned(), kind: PrincipalKind::Service, roles: vec![Role::SchemaAdmin] }))
            .collect(),
    };
    Ok(ServeOptions { bind: address, bootstrap, anonymous_loopback: loopback, label: "yyds" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use database::{
        cluster::topology::ShardMap,
        execution::cluster::{KeyValueExecutor, LocalExecutor},
        server::yydb::WireServe,
        storage::cluster_kv::MemoryShard,
        types::cluster::{Error as ClusterError, FenceToken, Result as ClusterResult, ShardEpoch, ShardId},
    };
    use database_protocol_yy::{Frame, MessageType, decode_error, encode_authenticate, encode_kv_get, encode_kv_put, read_frame, write_frame};
    use std::{
        collections::HashMap,
        net::TcpStream,
        sync::{Mutex as StdMutex, OnceLock},
        time::Duration,
    };

    fn test_factory() -> Arc<KvBackendFactory> {
        let routing = ShardMap::new(ShardEpoch(1), vec![ShardId("local-0".into())]).unwrap();
        let shard_id = ShardId("local-0".into());
        let shard = MemoryShard::new();
        let mut shards = HashMap::new();
        shards.insert(shard_id, shard);
        let executor = LocalExecutor::new(routing, shards, Vec::new()).unwrap();
        Arc::new(KvBackendFactory::new(Arc::new(executor), None))
    }

    fn options_with_tokens(tokens: &[&str]) -> ServeOptions {
        ServeOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            bootstrap: AuthBootstrap {
                tokens: tokens
                    .iter()
                    .map(|token| {
                        (
                            (*token).to_owned(),
                            Principal { name: "wire".to_owned(), kind: PrincipalKind::Service, roles: vec![Role::SchemaAdmin] },
                        )
                    })
                    .collect(),
            },
            anonymous_loopback: false,
            label: "yyds",
        }
    }

    fn env_guard() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    fn with_env<F: FnOnce()>(name: &str, value: Option<&str>, body: F) {
        let _guard = env_guard().lock().unwrap();
        let previous = std::env::var_os(name);
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        body();
        unsafe {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn bootstrap_tokens_absent_is_refused_on_non_loopback() {
        with_env("YYDS_AUTH_TOKENS", None, || {
            let error = match serve_options("0.0.0.0:7700".parse().unwrap()) {
                Ok(_) => panic!("non-loopback bind without tokens must be refused"),
                Err(error) => error,
            };
            assert!(error.contains("non-loopback"), "error should mention non-loopback bind requirement, got: {error}");
        });
    }

    #[test]
    fn bootstrap_tokens_parses_semicolon_separated_list() {
        with_env("YYDS_AUTH_TOKENS", Some("t-alpha;t-beta; t-gamma"), || {
            let options = serve_options("127.0.0.1:7700".parse().unwrap()).unwrap();
            let tokens: Vec<&str> = options.bootstrap.tokens.iter().map(|(token, _)| token.as_str()).collect();
            assert_eq!(tokens, vec!["t-alpha", "t-beta", "t-gamma"]);
            assert!(options.anonymous_loopback, "loopback keeps anonymous dev mode");
        });
    }

    #[test]
    fn bootstrap_tokens_blank_env_is_rejected() {
        with_env("YYDS_AUTH_TOKENS", Some(";;   ;"), || {
            let error = match serve_options("127.0.0.1:7700".parse().unwrap()) {
                Ok(_) => panic!("blank token list must be refused"),
                Err(error) => error,
            };
            assert!(error.contains("parsed to no entries"));
        });
    }

    /// 每个命令都返回存储错误的后端，用于验证错误类别不被压成协议错误。
    #[derive(Debug)]
    struct FailingExecutor;

    impl KeyValueExecutor for FailingExecutor {
        fn execute(&self, _fence: Option<&FenceToken>, _command: KeyValueCommand) -> ClusterResult<KeyValueResult> {
            Err(ClusterError::Corrupt("shard payload is unreadable"))
        }
    }

    #[test]
    fn storage_errors_keep_their_category_on_the_wire() {
        let factory = Arc::new(KvBackendFactory::new(Arc::new(FailingExecutor), None));
        let serve = WireServe::start(options_with_tokens(&["t-secret"]), factory).expect("start wire server");
        let mut stream = TcpStream::connect(serve.address()).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");

        write_frame(&mut stream, &Frame::new(MessageType::Hello, 1, Vec::new())).expect("write hello");
        assert_eq!(read_frame(&mut stream).expect("read hello ok").msg_type, MessageType::HelloOk);
        write_frame(&mut stream, &Frame::new(MessageType::Authenticate, 2, encode_authenticate("t-secret"))).expect("write authenticate");
        assert_eq!(read_frame(&mut stream).expect("read authenticate ok").msg_type, MessageType::AuthenticateOk);

        write_frame(&mut stream, &Frame::new(MessageType::KvGet, 3, encode_kv_get("k1"))).expect("write kv get");
        let error = read_frame(&mut stream).expect("read error frame");
        assert_eq!(error.msg_type, MessageType::Error);
        let wire = decode_error(&error.body).expect("decode error body");
        assert_eq!(wire.code, "database.shard.corrupt", "the storage failure category must survive the wire frame");
        assert_eq!(wire.operation, "io");
        assert!(!wire.retryable);

        serve.stop().expect("stop wire server");
    }

    #[test]
    fn tcp_roundtrip_speaks_the_shared_yy_protocol() {
        let serve = WireServe::start(options_with_tokens(&["t-secret"]), test_factory()).expect("start wire server");
        let mut stream = TcpStream::connect(serve.address()).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");

        write_frame(&mut stream, &Frame::new(MessageType::Hello, 1, Vec::new())).expect("write hello");
        let hello = read_frame(&mut stream).expect("read hello ok");
        assert_eq!(hello.msg_type, MessageType::HelloOk);
        assert_eq!(hello.request_id, 1);

        let authenticate = Frame::new(MessageType::Authenticate, 2, encode_authenticate("t-secret"));
        assert_eq!(authenticate.msg_type.as_u16(), 19, "Authenticate must stay at the shared code 19");
        write_frame(&mut stream, &authenticate).expect("write authenticate");
        let authenticated = read_frame(&mut stream).expect("read authenticate ok");
        assert_eq!(authenticated.msg_type, MessageType::AuthenticateOk);
        assert!(!authenticated.body.is_empty(), "AuthenticateOk carries the opaque session token");

        write_frame(&mut stream, &Frame::new(MessageType::KvPut, 3, encode_kv_put("k1", b"v1"))).expect("write kv put");
        assert_eq!(read_frame(&mut stream).expect("read kv put ok").msg_type, MessageType::KvPutOk);

        write_frame(&mut stream, &Frame::new(MessageType::KvGet, 4, encode_kv_get("k1"))).expect("write kv get");
        let got = read_frame(&mut stream).expect("read kv get ok");
        assert_eq!(got.msg_type, MessageType::KvGetOk);
        assert_eq!(got.request_id, 4);

        serve.stop().expect("stop wire server");
    }

    #[test]
    fn wrong_token_replies_with_the_structured_auth_code() {
        let serve = WireServe::start(options_with_tokens(&["t-secret"]), test_factory()).expect("start");
        let mut stream = TcpStream::connect(serve.address()).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");

        let authenticate = Frame::new(MessageType::Authenticate, 1, encode_authenticate("wrong-token"));
        write_frame(&mut stream, &authenticate).expect("write authenticate");
        let response = read_frame(&mut stream).expect("read error");
        assert_eq!(response.msg_type, MessageType::Error);
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("database.auth.token_invalid"), "error body should carry the structured auth code, got: {body}");

        serve.stop().expect("stop wire server");
    }
}
