#![doc = include_str!("readme.md")]
use crate::lsp::JavaLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Java semantics (Stdio).
pub async fn serve_java_mcp(vfs: MemoryVfs) {
    let service = JavaLanguageService::new(vfs, data_text_java::language::JavaLanguage::default());
    let server = language_mcp::McpServer::new(service);

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
