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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbrain_core::index::{
    index_workspace, parse_links_with_ranges, ReferenceResolution, WorkspaceIndex,
};
use dbrain_core::model::{NodeId, RelPath, SymbolEntry, SymbolKind};
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
}

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
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
        })
    }

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

                let result = tokio::task::spawn_blocking(move || index_workspace(&root)).await;

                match result {
                    Ok(index) => {
                        let msg = format!(
                            "developer-brain: indexed {} nodes, {} edges",
                            index.graph.node_count(),
                            index.graph.edge_count(),
                        );
                        let open_uris: Vec<Url> = {
                            let mut s = state_arc.write().await;
                            let uris = s.docs.keys().cloned().collect();
                            s.index = Some(index);
                            uris
                        };
                        client.log_message(MessageType::INFO, msg).await;
                        for uri in open_uris {
                            push_diagnostics(&uri, &state_arc, &client).await;
                        }
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

    // --- DocumentLink（2-3） ---

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let s = self.state.read().await;
        let (Some(index), Some(content), Some(root)) =
            (&s.index, s.docs.get(&params.text_document.uri), &s.root)
        else {
            return Ok(None);
        };

        let result = parse_links_with_ranges(content)
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

    // --- Hover（2-5） ---
    //
    // [[...]] にカーソルを置くと、リンク先の概要を Markdown で表示する。
    // ドキュメントリンクは見出し一覧、コードシンボルは種別・修飾名・ファイルを出す。

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pos = params.text_document_position_params.position;
        let s = self.state.read().await;
        let (Some(index), Some(content)) = (
            &s.index,
            s.docs
                .get(&params.text_document_position_params.text_document.uri),
        ) else {
            return Ok(None);
        };

        let found = parse_links_with_ranges(content)
            .into_iter()
            .find(|(_, range)| pos_in_range(pos, core_range_to_lsp(*range, content)));

        let Some((reference, link_range)) = found else {
            return Ok(None);
        };

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

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text,
            }),
            // リンク範囲をそのまま返すことで、カーソルを動かしてもすぐ消えない。
            range: Some(core_range_to_lsp(link_range, content)),
        }))
    }
}

// ---------------------------------------------------------------------------
// 診断（2-4）
// ---------------------------------------------------------------------------

async fn push_diagnostics(uri: &Url, state: &Arc<RwLock<State>>, client: &Client) {
    let diags = {
        let s = state.read().await;
        let (Some(index), Some(content)) = (&s.index, s.docs.get(uri)) else {
            return;
        };
        compute_diagnostics(content, index)
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
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
