#![doc = include_str!("readme.md")]

use crate::lsp::ProtobufLanguageService;
use language_server::MemoryVfs;

/// Starts an MCP server for the Protobuf language.
pub async fn serve_protobuf_mcp(vfs: MemoryVfs) {
    let service = ProtobufLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
