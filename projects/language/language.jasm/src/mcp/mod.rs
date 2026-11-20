#![doc = include_str!("readme.md")]
use crate::lsp::JasmLanguageService;
use language_server::MemoryVfs;

/// Serves the Jasm MCP service.
pub async fn serve_jasm_mcp(vfs: MemoryVfs) {
    let service = JasmLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
