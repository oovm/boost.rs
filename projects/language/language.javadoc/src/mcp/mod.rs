#![doc = include_str!("readme.md")]
use crate::lsp::JavadocLanguageService;
use language_server::MemoryVfs;

/// Starts the MCP server (Stdio) for Javadoc semantics.
pub async fn serve_javadoc_mcp(vfs: MemoryVfs) {
    {
        let service = JavadocLanguageService::new(vfs);
        let server = language_mcp::McpServer::new(service);

        let reader = tokio::io::BufReader::new(tokio::io::stdin());
        let writer = tokio::io::BufWriter::new(tokio::io::stdout());
        server.run(reader, writer).await.unwrap()
    }
}
