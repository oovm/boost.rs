#![doc = include_str!("readme.md")]
use language_server::MemoryVfs;

#[cfg(feature = "io-std")]
use crate::lsp::TypescriptLanguageService;

/// Start an MCP server for TypeScript semantics (Stdio).
#[cfg(feature = "io-std")]
pub async fn serve_typescript_mcp(vfs: MemoryVfs) {
    let service = TypescriptLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}

/// Start an MCP server for TypeScript semantics (Stdio).
#[cfg(not(feature = "io-std"))]
pub async fn serve_typescript_mcp(_vfs: MemoryVfs) {
    panic!("MCP server requires io-std feature")
}
