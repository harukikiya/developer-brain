//! # Developer Brain Language Server
//!
//! ## そもそも LSP とは？
//!
//! LSP（Language Server Protocol）は、エディタと「言語の賢い処理を行う
//! 別プログラム（= language server）」が JSON-RPC でやり取りするための
//! 共通規約です。補完・定義ジャンプ・ホバー・エラー表示などを、
//! エディタごとに作り直さずに済むよう Microsoft が定めました。
//!
//! ここではそのサーバ側を Rust（tower-lsp クレート）で実装します。
//! エディタ（VS Code 拡張）がクライアント、このプログラムがサーバです。
//! 両者は **標準入出力（stdio）** を通じてメッセージを送り合います。
//!
//! ## このクレートの役割は「薄い通訳」
//!
//! 索引やリンク解決の本体は `dbrain-core` 側にあります。ここは
//! 「LSP のリクエストを受け取り、コアに渡し、結果を LSP の形に直して返す」
//! という通訳に徹します。賢さはコアに、礼儀作法（プロトコル）はここに。
//!
//! ## M2 の実装内容
//!
//! | サブタスク | 機能 |
//! |-----------|------|
//! | 2-1+2-2 | 状態管理・索引読み込み・ドキュメント追跡 |
//! | 2-3 | DocumentLink + GotoDefinition |
//! | 2-4 | 診断（Dangling / Ambiguous の波線）|
//! | 2-5 | Hover |
//! | 2-6 | `[[` 補完 |
//! | 2-7 | CodeLens（逆参照数）|

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dbrain_core::index::{index_workspace, WorkspaceIndex};
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

// ---------------------------------------------------------------------------
// 共有状態
// ---------------------------------------------------------------------------

/// LSP サーバの共有状態。`Arc<RwLock<State>>` で複数の非同期ハンドラから安全に共有する。
///
/// なぜ `RwLock`?  LSP ハンドラは async で並行実行される。
/// hover/documentLink などの読み取りが多く、didChange 等の書き込みは少ないため、
/// 「読み取りは並行 OK、書き込みは排他」な RwLock が Mutex より効率的。
struct State {
    /// ワークスペースのルートディレクトリ（`initialize` で確定する）。
    root: Option<PathBuf>,
    /// 索引結果（`initialized` 後のバックグラウンドタスクで確定する）。
    /// None の間はリクエストを受けても「まだ索引中」として空応答する。
    index: Option<WorkspaceIndex>,
    /// 現在エディタで開いているファイルの内容バッファ（URI → テキスト全文）。
    /// FULL 同期なので didChange のたびに全文で上書きする。
    docs: HashMap<Url, String>,
}

impl State {
    fn new() -> Self {
        Self {
            root: None,
            index: None,
            docs: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// サーバ本体
// ---------------------------------------------------------------------------

/// LSP サーバ本体。`client` でエディタに話しかけ、`state` で索引を保持する。
struct Backend {
    client: Client,
    state: Arc<RwLock<State>>,
}

/// tower-lsp が要求する `Debug` を最小限で実装（state の中身は非 Debug な型を含む）。
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// LSP ハンドラ
// ---------------------------------------------------------------------------

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// 握手。クライアントが最初に一度だけ呼ぶ。
    ///
    /// サーバは「自分は何者で、どんな機能(capabilities)を提供できるか」を
    /// 返す。ここで宣言した機能だけがエディタから呼ばれるようになる。
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // ワークスペースのルートを記録する。
        // root_uri が無いケース（単一ファイルモード等）はルートなしのまま起動する。
        let root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok());
        {
            let mut s = self.state.write().await;
            s.root = root;
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "developer-brain-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // FULL: 変更のたびファイル全文を受け取る（M4 で増分に昇格予定）。
                // INCREMENTAL より実装がシンプルで、差分適用ロジックが不要になる。
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                // M2 で順次追加する capability はここに書き足す（2-3 以降）。
                ..Default::default()
            },
        })
    }

    /// 握手完了の通知。ここでバックグラウンド索引を開始する。
    ///
    /// なぜ `initialized` で索引を始めるのか?
    /// `initialize` の応答を返す前に重い処理をするとエディタがタイムアウトするため、
    /// 握手完了後（`initialized`）に非同期で開始するのが LSP の作法。
    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "developer-brain-lsp initialized")
            .await;

        let root = {
            let s = self.state.read().await;
            s.root.clone()
        };

        if let Some(root) = root {
            let state_arc = Arc::clone(&self.state);
            let client = self.client.clone();

            tokio::spawn(async move {
                client
                    .log_message(MessageType::INFO, "developer-brain: indexing workspace...")
                    .await;

                // index_workspace は CPU バウンドな同期処理。
                // spawn_blocking で OS スレッドプールに委ねることで、
                // 非同期エグゼキュータ（tokio のスレッドプール）をブロックしない。
                let result = tokio::task::spawn_blocking(move || index_workspace(&root)).await;

                match result {
                    Ok(index) => {
                        let msg = format!(
                            "developer-brain: indexed {} nodes, {} edges",
                            index.graph.node_count(),
                            index.graph.edge_count(),
                        );
                        {
                            let mut s = state_arc.write().await;
                            s.index = Some(index);
                        }
                        client.log_message(MessageType::INFO, msg).await;
                    }
                    Err(e) => {
                        client
                            .log_message(
                                MessageType::ERROR,
                                format!("developer-brain: indexing failed: {e}"),
                            )
                            .await;
                    }
                }
            });
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // --- ドキュメント追跡（2-2） ---
    // FULL 同期なので各ハンドラは単純：開いたら追加・変更したら上書き・閉じたら除去。

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let mut s = self.state.write().await;
        s.docs
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL 同期なので content_changes は要素 1 個（ファイル全文）のはず。
        // 複数届いた場合は最後が最新なので last() を使う。
        if let Some(change) = params.content_changes.into_iter().last() {
            let mut s = self.state.write().await;
            s.docs.insert(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut s = self.state.write().await;
        s.docs.remove(&params.text_document.uri);
    }
}

// ---------------------------------------------------------------------------
// エントリポイント
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        state: Arc::new(RwLock::new(State::new())),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
