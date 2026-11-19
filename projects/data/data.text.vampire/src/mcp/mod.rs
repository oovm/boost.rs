#![doc = include_str!("readme.md")]
use crate::lsp::VampireLanguageService;
use language_server::MemoryVfs;

/// Starts the Vampire MCP service
pub async fn serve_vampire_mcp(vfs: MemoryVfs) {
    let service = VampireLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
