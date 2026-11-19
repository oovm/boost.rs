#![doc = include_str!("readme.md")]
use crate::lsp::NimLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Nim semantics (Stdio).
pub async fn serve_nim_mcp(vfs: MemoryVfs) {
    let service = NimLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
