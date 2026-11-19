#![doc = include_str!("readme.md")]
use crate::lsp::SolidityLanguageService;
use language_server::MemoryVfs;

/// Starts the Solidity MCP service.
pub async fn serve_solidity_mcp(vfs: MemoryVfs) {
    let service = SolidityLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
