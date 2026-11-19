#![doc = include_str!("readme.md")]
use language_mcp::McpServer;
use language_server::{Vfs, service::LanguageService};

/// Serves Less MCP.
pub async fn serve_less_mcp<V: Vfs + Send + Sync + 'static + language_server::DocumentSession>(
    vfs: V,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    crate::lsp::LessLanguageService<V>: LanguageService,
{
    let service = crate::lsp::LessLanguageService::new(vfs);
    let server = McpServer::new(service);
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::BufWriter::new(tokio::io::stdout());
    server.run(reader, writer).await.unwrap();
    Ok(())
}
