#![doc = include_str!("readme.md")]
use crate::lsp::HaskellLanguageService;
use language_server::MemoryVfs;

/// Serves the Haskell Model Context Protocol (MCP) server.
pub async fn serve_haskell_mcp(vfs: MemoryVfs) {
    let service = HaskellLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
