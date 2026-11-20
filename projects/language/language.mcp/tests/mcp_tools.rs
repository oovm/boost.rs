//! MCP 工具调用只做协议投影：返回值必须来自语言服务的语义输出，而不是占位空数组。

use core::range::Range;
use data_text_core::{
    ElementType, Language, RedNode, TokenType, UniversalElementRole, UniversalTokenRole,
    vfs::MemoryVfs,
};
use language_mcp::{JsonRpcRequest, McpServer};
use language_server::{
    WorkspaceManager, service::LanguageService,
    types::{CompletionItem, Diagnostic, Hover, LocationRange, StructureItem},
};
use serde_json::{Value, json};
use std::future::Future;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TestToken {
    End,
}

impl TokenType for TestToken {
    type Role = UniversalTokenRole;
    const END_OF_STREAM: Self = TestToken::End;

    fn role(&self) -> Self::Role {
        UniversalTokenRole::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TestElement {
    Root,
}

impl ElementType for TestElement {
    type Role = UniversalElementRole;

    fn role(&self) -> Self::Role {
        UniversalElementRole::Root
    }
}

impl From<TestToken> for TestElement {
    fn from(_: TestToken) -> Self {
        TestElement::Root
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TestLanguage;

impl Language for TestLanguage {
    const NAME: &'static str = "test";
    type TokenType = TestToken;
    type ElementType = TestElement;
    type TypedRoot = ();
}

struct TestService {
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}

impl TestService {
    fn new() -> Self {
        Self { vfs: MemoryVfs::new(), workspace: WorkspaceManager::default() }
    }
}

impl LanguageService for TestService {
    type Lang = TestLanguage;
    type Vfs = MemoryVfs;

    fn vfs(&self) -> &Self::Vfs {
        &self.vfs
    }

    fn workspace(&self) -> &WorkspaceManager {
        &self.workspace
    }

    fn with_root<R, F>(&self, _uri: &str, _f: F) -> impl Future<Output = Option<R>> + Send
    where
        R: Send,
        F: FnOnce(RedNode<'_, Self::Lang>) -> R + Send,
    {
        async { None }
    }

    fn hover(&self, _uri: &str, range: Range<usize>) -> impl Future<Output = Option<Hover>> + Send + '_ {
        async move { Some(Hover { contents: "test hover".to_string(), range: Some(range) }) }
    }

    fn definition(&self, _uri: &str, _range: Range<usize>) -> impl Future<Output = Vec<LocationRange>> + Send + '_ {
        async { vec![LocationRange { uri: "file:///def".into(), range: Range { start: 3, end: 6 } }] }
    }

    fn references(&self, _uri: &str, _range: Range<usize>) -> impl Future<Output = Vec<LocationRange>> + Send + '_ {
        async { vec![LocationRange { uri: "file:///ref".into(), range: Range { start: 10, end: 13 } }] }
    }

    fn diagnostics(&self, _uri: &str) -> impl Future<Output = Vec<Diagnostic>> + Send + '_ {
        async {
            vec![Diagnostic {
                range: Range { start: 0, end: 1 },
                severity: None,
                code: None,
                source: None,
                message: "boom".to_string(),
            }]
        }
    }

    fn completion(&self, _uri: &str, _offset: usize) -> impl Future<Output = Vec<CompletionItem>> + Send + '_ {
        async {
            vec![CompletionItem { label: "item".to_string(), kind: None, detail: None, documentation: None, insert_text: None }]
        }
    }

    fn document_symbols<'a>(&'a self, _uri: &'a str) -> impl Future<Output = Vec<StructureItem>> + Send + 'a {
        async {
            vec![StructureItem {
                name: "root".to_string(),
                detail: None,
                role: UniversalElementRole::Root,
                range: Range { start: 0, end: 1 },
                selection_range: Range { start: 0, end: 1 },
                deprecated: false,
                children: vec![],
            }]
        }
    }
}

async fn call_tool(server: &McpServer<TestService>, name: &str, arguments: Value) -> Value {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: "tools/call".to_string(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let response = server.handle_request(request).await;
    assert!(response.error.is_none(), "tool `{name}` failed: {:?}", response.error);
    let text = response.result.expect("tool result")["content"][0]["text"].as_str().expect("tool result text").to_string();
    serde_json::from_str(&text).expect("tool result payload")
}

#[tokio::test]
async fn definition_projects_service_locations() {
    let server = McpServer::new(TestService::new());
    let result = call_tool(&server, "definition", json!({ "uri": "memory:///a", "offset": 4 })).await;

    assert_eq!(result[0]["uri"], "file:///def");
    assert_eq!(result[0]["range"]["start"], 3);
    assert_eq!(result[0]["range"]["end"], 6);
}

#[tokio::test]
async fn references_projects_service_locations() {
    let server = McpServer::new(TestService::new());
    let result = call_tool(&server, "references", json!({ "uri": "memory:///a", "offset": 4 })).await;

    assert_eq!(result[0]["uri"], "file:///ref");
    assert_eq!(result[0]["range"]["start"], 10);
    assert_eq!(result[0]["range"]["end"], 13);
}

#[tokio::test]
async fn diagnostics_projects_service_messages() {
    let server = McpServer::new(TestService::new());
    let result = call_tool(&server, "diagnostics", json!({ "uri": "memory:///a" })).await;

    assert_eq!(result[0]["message"], "boom");
    assert_eq!(result[0]["range"]["start"], 0);
}

#[tokio::test]
async fn completion_projects_service_items() {
    let server = McpServer::new(TestService::new());
    let result = call_tool(&server, "completion", json!({ "uri": "memory:///a", "offset": 0 })).await;

    assert_eq!(result[0]["label"], "item");
}

#[tokio::test]
async fn symbols_projects_service_structure() {
    let server = McpServer::new(TestService::new());
    let result = call_tool(&server, "symbols", json!({ "uri": "memory:///a" })).await;

    assert_eq!(result[0]["name"], "root");
    assert_eq!(result[0]["range"]["end"], 1);
}

#[tokio::test]
async fn set_file_content_writes_the_vfs() {
    let server = McpServer::new(TestService::new());
    let result = call_tool(&server, "set_file_content", json!({ "uri": "memory:///a", "content": "alpha" })).await;

    assert_eq!(result["bytes"], 5);
    assert!(server.service.source_for_uri("memory:///a").is_some(), "document must be readable after set_file_content");
}
