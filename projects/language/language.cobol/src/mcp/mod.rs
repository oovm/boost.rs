//! MCP support for COBOL.
use crate::lsp::CobolLanguageService;
use language_server::MemoryVfs;

/// Serves the COBOL Model Context Protocol (MCP) server.
pub async fn serve_cobol_mcp(vfs: MemoryVfs) {
    let service = CobolLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
