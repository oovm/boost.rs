#![doc = include_str!("readme.md")]
use crate::lsp::IdlLanguageService;
use language_server::MemoryVfs;

pub async fn serve_idl_mcp(vfs: MemoryVfs) {
    let service = IdlLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
