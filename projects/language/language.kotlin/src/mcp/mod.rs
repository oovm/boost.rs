#![doc = include_str!("readme.md")]
use crate::lsp::KotlinLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Kotlin semantics (Stdio).
pub async fn serve_kotlin_mcp(_vfs: MemoryVfs) {
    let service = KotlinLanguageService::new(data_text_kotlin::language::KotlinLanguage::default());
    let server = language_mcp::McpServer::new(service);

    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
