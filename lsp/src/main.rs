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
//! ## 実装内容
//!
//! | サブタスク | 機能 |
//! |-----------|------|
//! | 2-1+2-2 | 状態管理・索引読み込み・ドキュメント追跡 |
//! | 2-3 | DocumentLink + GotoDefinition |
//! | 2-4 | 診断（Dangling / Ambiguous の波線）|
//! | 2-5 | Hover |
//! | 2-6 | `[[` 補完 |
//! | 2-7 | CodeLens（逆参照数）|
//! | 5-3 | 用語集の仮想リンク（DocumentLink + Hover）|

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbrain_core::index::{
    find_term_mentions, index_workspace, parse_links_with_ranges, ReferenceResolution,
    WorkspaceIndex,
};
use dbrain_core::model::{Edge, NodeId, RelPath, SymbolEntry, SymbolKind};
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

// ---------------------------------------------------------------------------
// 共有状態
// ---------------------------------------------------------------------------

/// LSP サーバの共有状態。`Arc<RwLock<State>>` で複数の非同期ハンドラから安全に共有する。
struct State {
    root: Option<PathBuf>,
    /// 索引結果。None = まだ索引中。
    index: Option<WorkspaceIndex>,
    /// 開いているファイルのテキストバッファ（URI → 全文）。FULL 同期で上書き。
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

struct Backend {
    client: Client,
    state: Arc<RwLock<State>>,
    /// 再索引の「変更世代」カウンタ（デバウンス用）。
    ///
    /// 変更トリガーのたびに +1 する。スケジュールされた再索引タスクは
    /// 起床時にこの値が自分の捕捉した世代と一致するかを確認し、
    /// 一致しなければ（より新しいトリガーが来ていれば）自分を破棄する。
    /// これで「連続トリガーの最後の 1 回だけ実行」を実現する（[`Backend::schedule_reindex`]）。
    reindex_generation: Arc<AtomicU64>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").finish_non_exhaustive()
    }
}

/// 再索引デバウンスの待機時間。連続した変更トリガーをこの時間まとめる。
const REINDEX_DEBOUNCE: Duration = Duration::from_millis(300);

impl Backend {
    /// 再索引を「デバウンス付き」でスケジュールする（M4 4-3）。
    ///
    /// 連続した保存やファイル変更で再索引が何度も走るのを防ぐ。仕組み:
    /// 1. 世代カウンタを +1 し、その新しい値（`my_gen`）を覚える。
    /// 2. [`REINDEX_DEBOUNCE`] だけ待つ。
    /// 3. 起床時にカウンタがまだ `my_gen` のままなら（後続トリガーが無ければ）
    ///    実際に再索引する。進んでいれば自分は破棄する。
    ///
    /// これで「連続トリガーの最後の 1 回だけが実行される」。待ち行列も
    /// チャンネルも要らず、`AtomicU64` 1 個で完結する。
    fn schedule_reindex(&self) {
        // fetch_add は加算前の値を返すので、+1 した新しい値が my_gen。
        let my_gen = self.reindex_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation = Arc::clone(&self.reindex_generation);
        let state = Arc::clone(&self.state);
        let client = self.client.clone();

        tokio::spawn(async move {
            tokio::time::sleep(REINDEX_DEBOUNCE).await;
            // 自分が最後のトリガーだった場合のみ実行する。
            // 後続トリガーが来ていれば generation は my_gen より大きい。
            if generation.load(Ordering::SeqCst) == my_gen {
                reindex(state, client).await;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// LSP ハンドラ
// ---------------------------------------------------------------------------

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
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
                // 単なる同期種別(Kind)ではなく Options を使うのは、保存通知(did_save)を
                // 受け取りたいから。Kind だけだと save 通知は届かない。
                // include_text=false: 保存時にファイル全文は不要（ディスクから読み直すため）。
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                // `[` をトリガーにすると `[[` を打った瞬間に補完が起動する。
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["[".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                // CodeLens: シンボル定義行に被参照数を表示する（resolve なし）。
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                // カスタムコマンド。エディタ側が "dbrain/graph" を呼ぶとグラフ JSON を返す。
                // workspace/executeCommand は LSP 標準の拡張メカニズムで、
                // コマンド名だけ登録しておけばエディタから任意に呼び出せる。
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "dbrain/graph".to_string(),
                        // シンボル ID からファイル位置を逆引きする。Webview のジャンプに使う。
                        "dbrain/symbolInfo".to_string(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "developer-brain-lsp initialized")
            .await;

        // 起動直後にバックグラウンドで初回索引を構築する。
        tokio::spawn(reindex(Arc::clone(&self.state), self.client.clone()));
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // --- ドキュメント追跡（2-2） ---

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        {
            let mut s = self.state.write().await;
            s.docs.insert(uri.clone(), params.text_document.text);
        }
        push_diagnostics(&uri, &self.state, &self.client).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Some(change) = params.content_changes.into_iter().last() {
            let mut s = self.state.write().await;
            s.docs.insert(uri.clone(), change.text);
        }
        push_diagnostics(&uri, &self.state, &self.client).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut s = self.state.write().await;
        s.docs.remove(&params.text_document.uri);
        drop(s);
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    // --- 増分更新（M4 4-1）: 保存時に再索引する ---
    //
    // タイプ中（did_change）はバッファ更新＋当該ドキュメントの診断のみで、索引は
    // 更新しない。保存時はディスクが最新になるので、ここで索引を作り直し、
    // シンボルの増減やリンク解決の変化を全ドキュメントの診断に反映する。
    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        self.schedule_reindex();
    }

    // --- 増分更新（M4 4-2）: エディタ外のファイル変更に追従する ---
    //
    // git checkout、外部エディタでの編集、ファイルの新規作成/削除など、
    // VS Code のドキュメント編集を経由しない変更はここに届く。
    // クライアント（拡張）側の FileSystemWatcher が監視対象を購読しているため、
    // 該当ファイルが変わるとこの通知が来る。保存時と同じく索引を作り直す。
    async fn did_change_watched_files(&self, _params: DidChangeWatchedFilesParams) {
        self.schedule_reindex();
    }

    // --- DocumentLink（2-3 / 5-3） ---
    //
    // [[...]] の明示リンクと、用語集の仮想リンク（D8）の両方を返す。
    // 仮想リンクは本文を書き換えず、エディタ上だけでリンクとして振る舞う。

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let s = self.state.read().await;
        let (Some(index), Some(content), Some(root)) =
            (&s.index, s.docs.get(&params.text_document.uri), &s.root)
        else {
            return Ok(None);
        };

        // [[...]] の明示リンク。
        let mut result: Vec<DocumentLink> = parse_links_with_ranges(content)
            .into_iter()
            .map(|(reference, range)| {
                let lsp_range = core_range_to_lsp(range, content);
                let target = ref_to_uri(&reference, index, root);
                DocumentLink {
                    range: lsp_range,
                    target,
                    tooltip: None,
                    data: None,
                }
            })
            .collect();

        // 用語集の仮想リンク（5-3）: 本文中の用語出現を DocumentLink に変換する。
        // find_term_mentions は既存 [[...]] 内を除外するので二重リンクにならない。
        if let Some(rel) = uri_to_rel_path(&params.text_document.uri, root) {
            for m in find_term_mentions(content, &index.glossary, &rel) {
                let Some(term) = index.glossary.get(m.term_index) else {
                    continue;
                };
                let target = Url::from_file_path(root.join(&term.path.0)).ok();
                result.push(DocumentLink {
                    range: core_range_to_lsp(m.range, content),
                    target,
                    // tooltip はリンクにカーソルを合わせたときに出る短い説明。
                    tooltip: Some(term.name.clone()),
                    data: None,
                });
            }
        }

        Ok(Some(result))
    }

    // --- GotoDefinition（2-3） ---

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let s = self.state.read().await;
        let (Some(index), Some(content), Some(root)) = (
            &s.index,
            s.docs
                .get(&params.text_document_position_params.text_document.uri),
            &s.root,
        ) else {
            return Ok(None);
        };

        let found = parse_links_with_ranges(content)
            .into_iter()
            .find(|(_, range)| pos_in_range(pos, core_range_to_lsp(*range, content)));

        let Some((reference, _)) = found else {
            return Ok(None);
        };

        Ok(ref_to_location(&reference, index, root).map(GotoDefinitionResponse::Scalar))
    }

    // --- Hover（2-5 / 5-3） ---
    //
    // 優先順位: [[...]] リンク → 用語集の仮想リンク。
    // [[...]] にカーソルを置くと、リンク先の概要を Markdown で表示する。
    // 用語集の仮想リンクでは用語名と定義抜粋を表示する。

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pos = params.text_document_position_params.position;
        let uri = &params.text_document_position_params.text_document.uri;
        let s = self.state.read().await;
        let (Some(index), Some(content), Some(root)) = (&s.index, s.docs.get(uri), &s.root) else {
            return Ok(None);
        };

        // --- 1. [[...]] リンクの Hover ---
        let found = parse_links_with_ranges(content)
            .into_iter()
            .find(|(_, range)| pos_in_range(pos, core_range_to_lsp(*range, content)));

        if let Some((reference, link_range)) = found {
            let text = match index.resolve_reference(&reference) {
                ReferenceResolution::DocResolved(path) => format_doc_hover(&path, index),
                ReferenceResolution::CodeResolved { id } => {
                    let Some(entry) = index.symbols.get(id) else {
                        return Ok(None);
                    };
                    format_code_hover(entry)
                }
                _ => return Ok(None),
            };
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: text,
                }),
                // リンク範囲をそのまま返すことで、カーソルを動かしてもすぐ消えない。
                range: Some(core_range_to_lsp(link_range, content)),
            }));
        }

        // --- 2. 用語集の仮想リンク Hover（5-3）---
        let Some(rel) = uri_to_rel_path(uri, root) else {
            return Ok(None);
        };
        let mention = find_term_mentions(content, &index.glossary, &rel)
            .into_iter()
            .find(|m| pos_in_range(pos, core_range_to_lsp(m.range, content)));

        let Some(m) = mention else {
            return Ok(None);
        };
        let Some(term) = index.glossary.get(m.term_index) else {
            return Ok(None);
        };

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("**{}**\n\n{}", term.name, term.definition),
            }),
            range: Some(core_range_to_lsp(m.range, content)),
        }))
    }

    // --- 補完（2-6） ---
    //
    // `[[` を打った瞬間に起動し、ドキュメント名とコードシンボルを候補として返す。
    // TextEdit で「`[[` の直後〜カーソル」を置換するため、途中まで打っていても
    // 正しく上書きされる。

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let pos = params.text_document_position.position;
        let s = self.state.read().await;
        let (Some(index), Some(content)) = (
            &s.index,
            s.docs.get(&params.text_document_position.text_document.uri),
        ) else {
            return Ok(None);
        };

        // カーソル行を取得し、`[[` の中にいるか確認する。
        let line = content.lines().nth(pos.line as usize).unwrap_or("");
        let cursor_byte = utf16_col_to_byte_offset(line, pos.character);
        let before_cursor = &line[..cursor_byte.min(line.len())];

        // 最後の `[[` を探し、`]]` で既に閉じられていれば補完しない。
        let Some(bracket_pos) = before_cursor.rfind("[[") else {
            return Ok(None);
        };
        let after_bracket = &before_cursor[bracket_pos + 2..];
        if after_bracket.contains("]]") {
            return Ok(None);
        }

        // `[[` 直後〜カーソル位置 を TextEdit の置換範囲にする。
        let bracket_end_utf16 = str_to_utf16_col(&line[..bracket_pos + 2]);
        let replace_range = Range {
            start: Position {
                line: pos.line,
                character: bracket_end_utf16,
            },
            end: pos,
        };

        let mut items: Vec<CompletionItem> = Vec::new();

        // ドキュメント候補（フルパス）。
        for doc_path in &index.doc_paths {
            items.push(CompletionItem {
                label: doc_path.clone(),
                kind: Some(CompletionItemKind::FILE),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range,
                    new_text: format!("{doc_path}]]"),
                })),
                ..Default::default()
            });
        }

        // ドキュメント短縮名（一意なもののみ）。
        for (stem, path_opt) in &index.doc_by_stem {
            let Some(full_path) = path_opt else { continue };
            items.push(CompletionItem {
                label: stem.clone(),
                kind: Some(CompletionItemKind::FILE),
                // detail にフルパスを出すことで候補一覧でファイルパスが見える。
                detail: Some(full_path.0.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range,
                    new_text: format!("{stem}]]"),
                })),
                ..Default::default()
            });
        }

        // コードシンボル候補（`file.rs@Mod::Sym` 形式）。
        for (_, entry) in index.symbols.iter() {
            let qualified = entry.descriptor.path.0.join("::");
            let label = format!("{}@{}", entry.descriptor.file.0, qualified);
            let kind = entry.descriptor.kind.map(symbol_kind_to_completion_kind);
            items.push(CompletionItem {
                label: label.clone(),
                kind,
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range,
                    new_text: format!("{label}]]"),
                })),
                ..Default::default()
            });
        }

        Ok(if items.is_empty() {
            None
        } else {
            Some(CompletionResponse::Array(items))
        })
    }

    // --- CodeLens（2-7） ---
    //
    // ソースファイル内の各シンボル定義行に「N 件の参照」を表示する。
    // グラフの References 辺を集計するだけなので O(辺数)。

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let s = self.state.read().await;
        let (Some(index), Some(root)) = (&s.index, &s.root) else {
            return Ok(None);
        };

        // URI をワークスペース相対パスに変換する。
        let Some(abs_path) = params.text_document.uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(rel) = abs_path.strip_prefix(root).ok() else {
            return Ok(None);
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // References 辺を走査してシンボル ID → 参照数のマップを作る。
        let mut ref_counts: std::collections::HashMap<dbrain_core::model::SymbolId, usize> =
            std::collections::HashMap::new();
        for edge in index.graph.edges() {
            if let Edge::References {
                to: NodeId::Symbol(id),
                ..
            } = edge
            {
                *ref_counts.entry(*id).or_insert(0) += 1;
            }
        }

        let lenses: Vec<CodeLens> = index
            .symbols
            .iter()
            .filter(|(_, e)| e.descriptor.file.0 == rel_str)
            .filter_map(|(id, entry)| {
                // 参照が 0 件（マップに無い）はレンズを出さない。
                let count = *ref_counts.get(&id)?;
                let range = Range {
                    start: Position {
                        line: entry.name_range.start.line,
                        character: entry.name_range.start.character,
                    },
                    end: Position {
                        line: entry.name_range.end.line,
                        character: entry.name_range.end.character,
                    },
                };
                Some(CodeLens {
                    range,
                    command: Some(Command {
                        title: format!("{count} 件の参照"),
                        command: String::new(), // 今はクリック動作なし（表示のみ）
                        arguments: None,
                    }),
                    data: None,
                })
            })
            .collect();

        Ok(if lenses.is_empty() {
            None
        } else {
            Some(lenses)
        })
    }

    // --- カスタムコマンド（3-1 / feature: symbol-jump + section-opt） ---

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            // グラフ JSON を返す。
            // 引数: [{include_sections: bool}]（省略時は false）
            // include_sections = false にすると Section ノードと Contains 辺を除外し、
            // 転送量と Webview の JS メモリを削減できる。CLI は true を渡す。
            "dbrain/graph" => {
                let include_sections = params
                    .arguments
                    .first()
                    .and_then(|v| v.get("include_sections"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let s = self.state.read().await;
                let json_str = match &s.index {
                    Some(index) => index.graph.to_json_string(include_sections),
                    None => r#"{"nodes":[],"edges":[]}"#.to_string(),
                };
                let value = serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);
                Ok(Some(value))
            }

            // symbol ノードのジャンプ先（ファイル・行・列）を返す。
            // 引数: [symbolId: number]（NodeId "symbol:N" の N）
            // Webview のノードクリックで symbol ジャンプに使う。
            "dbrain/symbolInfo" => {
                let sym_id = params
                    .arguments
                    .first()
                    .and_then(|v| v.as_u64())
                    .map(|n| dbrain_core::model::SymbolId(n as u32));

                let Some(id) = sym_id else {
                    return Ok(None);
                };
                let s = self.state.read().await;
                let Some(index) = &s.index else {
                    return Ok(None);
                };
                let Some(entry) = index.symbols.get(id) else {
                    return Ok(None);
                };

                Ok(Some(serde_json::json!({
                    "file": entry.descriptor.file.0,
                    "line": entry.name_range.start.line,
                    "character": entry.name_range.start.character,
                })))
            }

            _ => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// 再索引（M4 4-1 / 4-4）
// ---------------------------------------------------------------------------

/// 再索引完了をクライアントへ知らせるカスタム通知（M4 4-4）。
///
/// LSP 標準にない独自メソッド。診断の push と同じく「応答を期待しない一方向
/// メッセージ」で、サーバ→クライアントへ送る。拡張側はこれを購読し、
/// グラフビューが開いていれば最新のグラフを取り直す。パラメータは持たない。
enum GraphChangedNotification {}

impl tower_lsp::lsp_types::notification::Notification for GraphChangedNotification {
    type Params = ();
    const METHOD: &'static str = "dbrain/graphChanged";
}

/// ワークスペースを索引し直し、結果を state に格納して全ドキュメントの診断を更新する。
///
/// 起動時（initialized）と保存時（did_save）の両方から呼ばれる共通処理。
/// `index_workspace` は CPU バウンドな同期処理なので `spawn_blocking` で
/// 非同期エグゼキュータをブロックしないようにする。
///
/// 所有権を引数で受け取る（参照ではない）のは、`tokio::spawn` に渡して
/// バックグラウンド実行するため。`Arc` と `Client` はどちらも安価に clone できる。
async fn reindex(state: Arc<RwLock<State>>, client: Client) {
    let root = {
        let s = state.read().await;
        s.root.clone()
    };
    let Some(root) = root else {
        return;
    };

    client
        .log_message(MessageType::INFO, "developer-brain: indexing workspace...")
        .await;

    let result = tokio::task::spawn_blocking(move || index_workspace(&root)).await;

    match result {
        Ok(index) => {
            let msg = format!(
                "developer-brain: indexed {} nodes, {} edges",
                index.graph.node_count(),
                index.graph.edge_count(),
            );
            // 索引を差し替えてから、開いている全ドキュメントの診断を更新する。
            // シンボルの増減やリンク解決の変化が、編集していないファイルの
            // 波線にも反映される（これが M4 の主目的）。
            let open_uris: Vec<Url> = {
                let mut s = state.write().await;
                let uris = s.docs.keys().cloned().collect();
                s.index = Some(index);
                uris
            };
            client.log_message(MessageType::INFO, msg).await;
            for uri in open_uris {
                push_diagnostics(&uri, &state, &client).await;
            }
            // グラフが変わった可能性があるので、開いている Webview に再取得を促す。
            client
                .send_notification::<GraphChangedNotification>(())
                .await;
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
}

// ---------------------------------------------------------------------------
// 診断（2-4）
// ---------------------------------------------------------------------------

async fn push_diagnostics(uri: &Url, state: &Arc<RwLock<State>>, client: &Client) {
    let diags = {
        let s = state.read().await;
        let (Some(index), Some(content), Some(root)) = (&s.index, s.docs.get(uri), &s.root) else {
            return;
        };
        let mut diags = compute_diagnostics(content, index);
        // 用語集ノートへの重複定義警告（5-4）。
        diags.extend(compute_glossary_diagnostics(uri, content, index, root));
        diags
    };
    client.publish_diagnostics(uri.clone(), diags, None).await;
}

fn compute_diagnostics(content: &str, index: &WorkspaceIndex) -> Vec<Diagnostic> {
    parse_links_with_ranges(content)
        .into_iter()
        .filter_map(|(reference, range)| {
            let lsp_range = core_range_to_lsp(range, content);
            let (msg, severity) = resolution_to_diagnostic(index.resolve_reference(&reference))?;
            Some(Diagnostic {
                range: lsp_range,
                severity: Some(severity),
                message: msg,
                source: Some("developer-brain".to_string()),
                ..Default::default()
            })
        })
        .collect()
}

/// 用語集ノートの重複定義を診断する（M5-4 Linter）。
///
/// 現在のファイルが `glossary/` 配下のノートであり、かつ同一用語名のノートが
/// 他にも存在する（[`WorkspaceIndex::duplicate_terms`] に登録されている）場合に
/// Warning 診断を返す。範囲は最初の行（見出し行）全体。
///
/// この診断は「定義が正しい場所」、つまり用語ノート自身に出す設計にしている。
/// どちらが正しい定義かはツールには判断できないので、両方のファイルに警告を出す。
fn compute_glossary_diagnostics(
    uri: &Url,
    content: &str,
    index: &WorkspaceIndex,
    root: &Path,
) -> Vec<Diagnostic> {
    let Some(rel) = uri_to_rel_path(uri, root) else {
        return Vec::new();
    };
    // glossary/ 配下のノートだけが対象。
    if rel.0.split('/').next() != Some("glossary") {
        return Vec::new();
    }
    let stem = std::path::Path::new(&rel.0)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() || !index.duplicate_terms.contains(&stem) {
        return Vec::new();
    }
    // 見出し行（最初の行）をハイライト範囲にする。
    let first_line = content.lines().next().unwrap_or("");
    let end_char: u32 = first_line.chars().map(|c| c.len_utf16() as u32).sum();
    vec![Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: end_char,
            },
        },
        severity: Some(DiagnosticSeverity::WARNING),
        message: format!("用語「{stem}」が複数のファイルで定義されています"),
        source: Some("developer-brain".to_string()),
        ..Default::default()
    }]
}

/// 解決結果を診断メッセージに変換する。解決できた場合は `None`（診断不要）。
fn resolution_to_diagnostic(res: ReferenceResolution) -> Option<(String, DiagnosticSeverity)> {
    match res {
        ReferenceResolution::DocResolved(_) | ReferenceResolution::CodeResolved { .. } => None,
        ReferenceResolution::DocAmbiguous => Some((
            "同名のドキュメントが複数あります。フルパスで指定してください".to_string(),
            DiagnosticSeverity::WARNING,
        )),
        ReferenceResolution::DocDangling => Some((
            "リンク先のドキュメントが見つかりません".to_string(),
            DiagnosticSeverity::WARNING,
        )),
        ReferenceResolution::CodeAmbiguous(ids) => Some((
            format!(
                "候補が {} 件あります。disambiguator で絞り込んでください",
                ids.len()
            ),
            DiagnosticSeverity::WARNING,
        )),
        ReferenceResolution::CodeDangling => Some((
            "シンボルが見つかりません".to_string(),
            DiagnosticSeverity::WARNING,
        )),
    }
}

// ---------------------------------------------------------------------------
// リンク解決ヘルパ（LSP アダプタ層）
// ---------------------------------------------------------------------------
//
// `WorkspaceIndex::resolve_reference` がコアで解決結果を返す。
// ここでは LSP 型（Url, Location）への変換だけを担う「薄い通訳」。

/// ドキュメント URI をワークスペース相対パス（[`RelPath`]）に変換する。
///
/// `find_term_mentions` はファイルを特定するために `current_path` を受け取るが、
/// LSP ハンドラが持つのは URI。この関数で橋渡しする。
/// URI がファイルパスでない場合や、root の外にある場合は `None`。
fn uri_to_rel_path(uri: &Url, root: &Path) -> Option<RelPath> {
    let abs = uri.to_file_path().ok()?;
    let rel = abs.strip_prefix(root).ok()?;
    Some(RelPath(rel.to_string_lossy().replace('\\', "/")))
}

/// 参照を `DocumentLink.target`（URI）に変換する。
///
/// コードシンボルは行精度のナビゲーションが必要なため GotoDefinition に委ねる。
fn ref_to_uri(reference: &Reference, index: &WorkspaceIndex, root: &Path) -> Option<Url> {
    match index.resolve_reference(reference) {
        ReferenceResolution::DocResolved(path) => Url::from_file_path(root.join(&path.0)).ok(),
        ReferenceResolution::CodeResolved { id } => {
            let entry = index.symbols.get(id)?;
            Url::from_file_path(root.join(&entry.descriptor.file.0)).ok()
        }
        _ => None,
    }
}

/// 参照を `Location`（URI + 行・列範囲）に変換する。GotoDefinition で使う。
fn ref_to_location(reference: &Reference, index: &WorkspaceIndex, root: &Path) -> Option<Location> {
    match index.resolve_reference(reference) {
        ReferenceResolution::DocResolved(path) => {
            let uri = Url::from_file_path(root.join(&path.0)).ok()?;
            Some(Location {
                uri,
                range: Range::default(),
            })
        }
        ReferenceResolution::CodeResolved { id } => {
            let entry = index.symbols.get(id)?;
            let uri = Url::from_file_path(root.join(&entry.descriptor.file.0)).ok()?;
            // name_range を使うことで定義全体ではなく名前部分にカーソルが移動する。
            // character は code.rs で byte_col_to_char_count により char 数に変換済み。
            let range = Range {
                start: Position {
                    line: entry.name_range.start.line,
                    character: entry.name_range.start.character,
                },
                end: Position {
                    line: entry.name_range.end.line,
                    character: entry.name_range.end.character,
                },
            };
            Some(Location { uri, range })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Hover コンテンツ生成（2-5）
// ---------------------------------------------------------------------------

/// ドキュメントリンクの Hover テキストを生成する。
///
/// グラフの Section ノードを走査して見出し一覧を得る。
/// 見出しがなければパスだけを表示する。
fn format_doc_hover(path: &RelPath, index: &WorkspaceIndex) -> String {
    let headings: Vec<&str> = index
        .graph
        .node_ids()
        .filter_map(|n| match n {
            NodeId::Section(p, h) if p == path => Some(h.as_str()),
            _ => None,
        })
        .collect();

    if headings.is_empty() {
        format!("**{}**", path.0)
    } else {
        let list = headings
            .iter()
            .map(|h| format!("- {h}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("**{}**\n\n{}", path.0, list)
    }
}

/// コードシンボルの Hover テキストを生成する。
///
/// 種別（fn / struct 等）・修飾名・ファイルパスを Markdown で組み立てる。
fn format_code_hover(entry: &SymbolEntry) -> String {
    let kind = entry
        .descriptor
        .kind
        .map(symbol_kind_str)
        .unwrap_or("symbol");
    let qualified = entry.descriptor.path.0.join("::");
    format!(
        "**{}** `{}`\n\n`{}`",
        kind, qualified, entry.descriptor.file.0
    )
}

/// [`SymbolKind`] を人が読みやすい短い文字列に変換する。
fn symbol_kind_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "fn",
        SymbolKind::Method => "fn",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Trait => "trait",
        SymbolKind::Type => "type",
        SymbolKind::Const => "const",
        SymbolKind::Static => "static",
        SymbolKind::Field => "field",
        SymbolKind::Variant => "variant",
        SymbolKind::Variable => "var",
        SymbolKind::Macro => "macro!",
        SymbolKind::Module => "mod",
    }
}

// ---------------------------------------------------------------------------
// 補完ヘルパ（2-6）
// ---------------------------------------------------------------------------

/// LSP の UTF-16 列番号を行テキスト内のバイトオフセットに変換する。
///
/// 補完の `position` は UTF-16 単位で届くが、Rust の文字列操作はバイト単位なので変換が要る。
fn utf16_col_to_byte_offset(line: &str, utf16_col: u32) -> usize {
    let mut col = 0u32;
    let mut byte_offset = 0;
    for ch in line.chars() {
        if col >= utf16_col {
            break;
        }
        col += ch.len_utf16() as u32;
        byte_offset += ch.len_utf8();
    }
    byte_offset
}

/// 文字列の先頭から末尾までの UTF-16 code unit 数を返す。
///
/// TextEdit の範囲計算で「`[[` の直後の UTF-16 列番号」を求めるために使う。
fn str_to_utf16_col(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// [`SymbolKind`] を LSP の [`CompletionItemKind`] に対応付ける。
fn symbol_kind_to_completion_kind(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::Function | SymbolKind::Method => CompletionItemKind::FUNCTION,
        SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait | SymbolKind::Type => {
            CompletionItemKind::CLASS
        }
        SymbolKind::Const | SymbolKind::Static => CompletionItemKind::CONSTANT,
        SymbolKind::Field => CompletionItemKind::FIELD,
        SymbolKind::Variant => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::Variable => CompletionItemKind::VARIABLE,
        SymbolKind::Macro => CompletionItemKind::KEYWORD,
        SymbolKind::Module => CompletionItemKind::MODULE,
    }
}

// ---------------------------------------------------------------------------
// 型エイリアス（型推論補助）
// ---------------------------------------------------------------------------

/// `parse_links_with_ranges` の要素型。handlers から参照を渡すときに使う。
type Reference = dbrain_core::model::Reference;

// ---------------------------------------------------------------------------
// UTF-16 位置変換（LSP アダプタ層の責務）
// ---------------------------------------------------------------------------

fn core_range_to_lsp(range: dbrain_core::model::Range, content: &str) -> Range {
    Range {
        start: core_pos_to_lsp(range.start, content),
        end: core_pos_to_lsp(range.end, content),
    }
}

/// コアの `model::Position`（UTF-8 char 数）を LSP の `Position`（UTF-16 code unit）に変換する。
///
/// `parse_links_with_ranges` が返す位置は UTF-8 char 数。LSP の規約は UTF-16 code unit 数。
/// BMP 外文字（絵文字等）は UTF-16 で 2 unit になるため、`char.len_utf16()` で積算する。
fn core_pos_to_lsp(pos: dbrain_core::model::Position, content: &str) -> Position {
    let line_str = content.lines().nth(pos.line as usize).unwrap_or("");
    let utf16_char: u32 = line_str
        .chars()
        .take(pos.character as usize)
        .map(|c| c.len_utf16() as u32)
        .sum();
    Position {
        line: pos.line,
        character: utf16_char,
    }
}

fn pos_in_range(pos: Position, range: Range) -> bool {
    if pos.line != range.start.line {
        return false;
    }
    pos.character >= range.start.character && pos.character < range.end.character
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
        reindex_generation: Arc::new(AtomicU64::new(0)),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
