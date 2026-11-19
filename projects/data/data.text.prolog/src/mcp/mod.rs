#![doc = include_str!("readme.md")]
use crate::lsp::PrologLanguageService;
use language_server::MemoryVfs;

pub async fn serve_prolog_mcp(vfs: MemoryVfs) {
    let service = PrologLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
