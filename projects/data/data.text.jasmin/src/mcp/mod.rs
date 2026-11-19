#![doc = include_str!("readme.md")]
use crate::lsp::JasminLanguageService;
use language_server::MemoryVfs;

/// Starts the Jasmin MCP server.
pub async fn serve_jasmin_mcp(vfs: MemoryVfs) {
    let service = JasminLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
