#![doc = include_str!("readme.md")]
#[cfg(feature = "mcp")]
use crate::lsp::TypstLanguageService;
#[cfg(feature = "mcp")]
use language_server::MemoryVfs;

/// Starts the Typst MCP server (Stdio).
#[cfg(feature = "mcp")]
pub async fn serve_typst_mcp(vfs: MemoryVfs) {
    let service = TypstLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
