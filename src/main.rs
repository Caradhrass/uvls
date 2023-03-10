#![allow(dead_code)]

use dashmap::DashMap;
use document::{AsyncDraft, Draft, DraftSync};
use flexi_logger::FileSpec;

use tokio::{join, spawn};

use document::*;
use log::info;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
mod document;

mod ast;
mod check;
mod color;
mod completion;
mod location;
mod parse;
mod query;
mod semantic;
mod smt;
mod util;
use semantic::Snapshot;
static VERSION: &str = "v0.0.10";
//The server core, request and respones handling
struct Backend {
    client: Client,
    coloring: Arc<color::State>,
    documents: Arc<DashMap<Url, AsyncDraft>>,
    semantic: Arc<semantic::Context>,
}
impl Backend {
    async fn sync_draft(
        &self,
        uri: &Url,
        sync: DraftSync,
        deadline: Option<Instant>,
    ) -> Option<Draft> {
        let mut draft = self.documents.get(uri).map(|d| d.clone())?;
        if let Some(deadline) = deadline {
            draft.sync(sync, deadline).await
        } else {
            draft.wait(sync).await
        }
    }
    async fn remove(&self, uri: &Url, by_editor: bool) {
        let time = Instant::now();
        if self
            .documents
            .remove_if(uri, |_, v| {
                by_editor || v.state != DocumentState::OwnedByEditor
            })
            .is_some()
        {
            self.semantic
                .documents
                .lock()
                .send_modify(|docs| docs.delete(uri, time));
        }
    }
    fn load(&self, uri: &Url) {
        if self
            .documents
            .get(uri)
            .map(|doc| doc.state == DocumentState::OwnedByEditor)
            .unwrap_or(false)
        {
            return;
        }
        let documents = self.documents.clone();
        let semantic = self.semantic.clone();
        let uri = uri.clone();

        tokio::task::spawn_blocking(move || {
            load_blocking(uri, &documents, &semantic);
        });
    }
    async fn snapshot(&self, uri: &Url, sync: bool) -> Option<(Draft, Snapshot)> {
        if let Some(draft) = self.sync_draft(uri, DraftSync::Tree, None).await {
            if sync {
                self.semantic
                    .snapshot_sync(uri, draft.revision())
                    .await
                    .map(|snap| (draft, snap))
            } else {
                self.semantic.snapshot(uri).await.map(|snap| (draft, snap))
            }
        } else {
            None
        }
    }
}
//load a file this is tricky because the editor can also load it at the same time
fn load_blocking(
    uri: Url,
    documents: &DashMap<Url, AsyncDraft>,
    semantic: &Arc<semantic::Context>,
) {
    if std::fs::File::open(uri.path())
        .and_then(|mut f| {
            let meta = f.metadata()?;
            let modified = meta.modified()?;
            if let Some(old) = documents.get(&uri) {
                if !old.state.can_update(&DocumentState::OwnedByOs(modified)) {
                    return Ok(());
                }
            }

            let mut data = String::new();
            f.read_to_string(&mut data)?;
            match documents.entry(uri.clone()) {
                dashmap::mapref::entry::Entry::Vacant(e) => {
                    e.insert(AsyncDraft::open(
                        data,
                        DocumentState::OwnedByOs(modified),
                        uri.clone(),
                        semantic.clone(),
                    ));
                }
                dashmap::mapref::entry::Entry::Occupied(mut e) => {
                    if e.get()
                        .state
                        .can_update(&DocumentState::OwnedByOs(modified))
                    {
                        e.insert(AsyncDraft::open(
                            data,
                            DocumentState::OwnedByOs(modified),
                            uri.clone(),
                            semantic.clone(),
                        ));
                    }
                }
            }
            Ok(())
        })
        .is_err()
    {
        info!("Failed to load file {}", uri);
    }
}
//load all files under given a path
fn load_all_blocking(
    path: &Path,
    documents: Arc<DashMap<Url, AsyncDraft>>,
    semantic: Arc<semantic::Context>,
) {
    for e in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .map(|e| e == std::ffi::OsStr::new("uvl"))
                .unwrap_or(false)
        })
    {
        let semantic = semantic.clone();
        let documents = documents.clone();

        load_blocking(
            Url::from_file_path(e.path()).unwrap(),
            &documents,
            &semantic,
        )
    }
}
fn shutdown_error() -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::InternalError,
        message: "".into(),
        data: None,
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, init_params: InitializeParams) -> Result<InitializeResult> {
        #[allow(deprecated)]
        let root_folder = init_params
            .root_path
            .as_deref()
            .or_else(|| init_params.root_uri.as_ref().map(|p| p.path()))
            .map(PathBuf::from);
        if let Some(root_folder) = root_folder {
            let documents = self.documents.clone();
            let semantic = self.semantic.clone();
            //cheap fix for better intial load, we should really use priority model to prefer
            //editor owned files
            let _ = spawn(async move {
                tokio::task::spawn_blocking(move || {
                    load_all_blocking(&root_folder, documents, semantic);
                })
                .await
            });
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: String::from("uvl lsp"),
                version: None,
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    all_commit_characters: None,
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions {
                                work_done_progress: None,
                            },
                            legend: SemanticTokensLegend {
                                token_types: color::token_types(),
                                token_modifiers: Vec::new(),
                            },
                            range: None,
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                        },
                    ),
                ),
                references_provider: Some(OneOf::Left(true)),

                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
        let watcher = FileSystemWatcher {
            glob_pattern: "**/*.uvl".to_string(),
            kind: None,
        };
        let reg = Registration {
            id: "watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers: vec![watcher],
            })
            .ok(),
        };
        if self.client.register_capability(vec![reg]).await.is_err() {
            info!("failed to initialize file watchers");
        }
    }
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        info!("received did_open");
        self.documents.insert(
            params.text_document.uri.clone(),
            AsyncDraft::open(
                params.text_document.text,
                DocumentState::OwnedByEditor,
                params.text_document.uri,
                self.semantic.clone(),
            ),
        );

        info!("done did_open");
    }
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut updated = false;
        let uri = params.text_document.uri.clone();
        if let Some(mut doc) = self.documents.get_mut(&params.text_document.uri) {
            doc.update(params, self.semantic.clone());
            updated = true;
        }
        if updated {
            self.client.publish_diagnostics(uri, vec![], None).await;
        }
        info!("done did_change");
    }
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        info!("received completion request");
        if let Some((draft, root)) = self
            .snapshot(&params.text_document_position.text_document.uri, false)
            .await
        {
            return Ok(Some(CompletionResponse::List(
                completion::compute_completions(root, &draft, params.text_document_position),
            )));
        }
        Ok(None)
    }
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        if let Some((draft, root)) = self.snapshot(&uri, true).await {
            Ok(location::goto_definition(
                &root,
                &draft,
                &params.text_document_position_params.position,
                uri,
            ))
        } else {
            Ok(None)
        }
    }
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        if let Some((draft, root)) = self.snapshot(&uri, true).await {
            Ok(location::find_references(
                &root,
                &draft,
                &params.text_document_position.position,
                uri,
            ))
        } else {
            return Ok(None);
        }
    }
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        if let Some((draft, root)) = self.snapshot(&uri, false).await {
            let color = self.coloring.clone();
            return Ok(match draft {
                Draft::Tree { source, tree, .. } => color.get(root, uri, tree, source),
                _ => {
                    unimplemented!()
                }
            })
            .map(|r| Some(SemanticTokensResult::Tokens(r)));
        }
        Ok(None)
    }
    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri = params.text_document.uri;
        if let Some((draft, root)) = self.snapshot(&uri, false).await {
            let color = self.coloring.clone();
            Ok(match draft {
                Draft::Tree { source, tree, .. } => Some(color.delta(root, uri, tree, source)),
                _ => {
                    unimplemented!()
                }
            })
        } else {
            Ok(None)
        }
    }
    async fn did_save(&self, _: DidSaveTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file saved!")
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file closed!")
            .await;
        self.remove(&params.text_document.uri, true).await;
        self.load(&params.text_document.uri);
    }
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        info!("file change {:?}", params);
        for i in params.changes {
            match i.typ {
                FileChangeType::CREATED => {
                    self.load(&i.uri);
                }
                FileChangeType::CHANGED => {
                    self.load(&i.uri);
                }
                FileChangeType::DELETED => {
                    self.remove(&i.uri, false).await;
                }
                _ => {}
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        self.semantic.shutdown.cancel();
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    //only needed for vscode auto update
    if std::env::args().any(|a| &a == "-v") {
        println!("{}", VERSION);
        return;
    }

    let _logger = flexi_logger::Logger::try_with_env_or_str("info")
        .expect("Log spec string broken")
        .log_to_file(
            FileSpec::default()
                .directory(std::env::temp_dir())
                .basename("UVLS")
                .suppress_timestamp()
                .suffix("log"),
        )
        .write_mode(flexi_logger::WriteMode::Async)
        .start()
        .expect("Failed to start logger");
    log_panics::init();
    info!("UVLS start");
    let (service, socket) = LspService::new(|client| {
        let documents = Arc::new(DashMap::new());
        let shutdown = CancellationToken::new();
        let semantic = semantic::create_handler(client.clone(), shutdown, documents.clone());
        Backend {
            semantic,
            documents,
            coloring: Arc::new(color::State::new()),
            client,
        }
    });

    join!(Server::new(stdin, stdout, socket).serve(service));
}
