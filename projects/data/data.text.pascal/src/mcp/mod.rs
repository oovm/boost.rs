#![doc = include_str!("readme.md")]
use crate::lsp::PascalLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Pascal semantics (Stdio).
pub async fn serve_pascal_mcp(vfs: MemoryVfs) {
    let service = PascalLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
