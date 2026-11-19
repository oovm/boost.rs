#![doc = include_str!("readme.md")]
use crate::lsp::PerlLanguageService;
use language_server::MemoryVfs;

/// Start an MCP server for Perl semantics (Stdio).
pub async fn serve_perl_mcp(vfs: MemoryVfs) {
    let service = PerlLanguageService::new(vfs);
    let server = language_mcp::McpServer::new(service);

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let reader = tokio::io::BufReader::new(stdin);

    server.run(reader, stdout).await.unwrap()
}
