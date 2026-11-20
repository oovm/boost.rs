#![doc = include_str!("readme.md")]
use crate::lsp::TomlLanguageService;
use language_server::MemoryVfs;

/// Starts the MCP server (Stdio) for TOML semantics.
pub async fn serve_toml_mcp(vfs: MemoryVfs) {
    let service = TomlLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);

    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();

    server.run(&mut reader, &mut writer).await.unwrap()
}
