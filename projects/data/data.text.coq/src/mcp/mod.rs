#![doc = include_str!("readme.md")]
use crate::lsp::CoqLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Coq semantics (Stdio).
pub async fn serve_coq_mcp(vfs: MemoryVfs) {
    let service = CoqLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
