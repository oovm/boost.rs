#![doc = include_str!("readme.md")]
use crate::lsp::StylusLanguageService;
use language_mcp::McpServer;
use language_server::Vfs;

pub async fn serve_stylus_mcp<V: Vfs + Send + Sync + 'static + language_server::DocumentSession>(vfs: V) {
    let service = StylusLanguageService::new(vfs);
    let server = McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap()
}
