use std::rc::Rc;

use dprint_core::plugins::process::start_parent_process_checker_task;
use tower_lsp::lsp_types::DidChangeTextDocumentParams;
use tower_lsp::lsp_types::DidCloseTextDocumentParams;
use tower_lsp::lsp_types::DidOpenTextDocumentParams;
use tower_lsp::lsp_types::DocumentFormattingParams;
use tower_lsp::lsp_types::DocumentRangeFormattingParams;
use tower_lsp::lsp_types::InitializeParams;
use tower_lsp::lsp_types::InitializeResult;
use tower_lsp::lsp_types::InitializedParams;
use tower_lsp::lsp_types::MessageType;
use tower_lsp::lsp_types::TextEdit;
use tower_lsp::Client;
use tower_lsp::LspService;
use tower_lsp::Server;

use crate::arg_parser::CliArgs;
use crate::environment::Environment;
use crate::plugins::PluginResolver;

pub async fn run_language_server<TEnvironment: Environment>(
  args: &CliArgs,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
) -> anyhow::Result<()> {
  let stdin = tokio::io::stdin();
  let stdout = tokio::io::stdout();

  let (service, socket) = LspService::new(|client| Backend { client });
  Server::new(stdin, stdout, socket).serve(service).await;

  Ok(())
}

struct Backend {
  client: Client,
}

#[tower_lsp::async_trait]
impl tower_lsp::LanguageServer for Backend {
  async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, tower_lsp::jsonrpc::Error> {
    if let Some(parent_id) = params.process_id {
      start_parent_process_checker_task(parent_id);
    }
    Ok(InitializeResult::default())
  }

  async fn initialized(&self, _: InitializedParams) {
    self.client.log_message(MessageType::INFO, "Server initialized.").await;
  }

  async fn shutdown(&self) -> Result<(), tower_lsp::jsonrpc::Error> {
    Ok(())
  }

  async fn did_open(&self, params: DidOpenTextDocumentParams) {
    // todo, keep track of
  }

  async fn did_change(&self, params: DidChangeTextDocumentParams) {
    // todo
  }

  async fn did_close(&self, params: DidCloseTextDocumentParams) {
    // todo
  }

  async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>, tower_lsp::jsonrpc::Error> {
    // todo
    Ok(None)
  }

  async fn range_formatting(&self, params: DocumentRangeFormattingParams) -> Result<Option<Vec<TextEdit>>, tower_lsp::jsonrpc::Error> {
    // todo
    Ok(None)
  }
}
