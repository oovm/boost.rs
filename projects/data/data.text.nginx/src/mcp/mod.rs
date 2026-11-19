#![doc = include_str!("readme.md")]
use crate::lsp::NginxLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Nginx semantics (Stdio).
pub async fn serve_nginx_mcp(vfs: MemoryVfs) {
    let service = NginxLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
