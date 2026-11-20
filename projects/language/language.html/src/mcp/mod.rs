#![doc = include_str!("readme.md")]
use crate::lsp::HtmlLanguageService;
use language_server::MemoryVfs;

/// Starts an MCP server for HTML using the provided virtual file system.
pub async fn serve_html_mcp(vfs: MemoryVfs) {
    let service = HtmlLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
