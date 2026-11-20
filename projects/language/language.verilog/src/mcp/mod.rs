#![doc = include_str!("readme.md")]
use data_text_verilog::VerilogLanguage;
use language_mcp::McpServer;
use language_server::{Vfs, service::LanguageService};

/// Serves Verilog MCP.
pub async fn serve_verilog_mcp<V: Vfs + Send + Sync + 'static + language_server::DocumentSession>(
    vfs: V,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    crate::lsp::VerilogLanguageService<V>: LanguageService,
{
    let service = crate::lsp::VerilogLanguageService::new(vfs);
    let _server = McpServer::new(service);
    Ok(())
}
