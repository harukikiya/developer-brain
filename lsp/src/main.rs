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
//! という通訳に徹します。賢さはコアに、礼儀作法（プロトコル）はここに、
//! と責務を分けておくと、後で MCP など別の入口を足すのが楽になります。
//!
//! ## M0 の到達点
//!
//! いまは `initialize`（握手）に応答するだけの最小サーバです。
//! M2 で `completion`（`[[` 補完）・`document_link`（リンクジャンプ）・
//! `hover`・`code_lens`（逆参照表示）をここに足していきます。

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// サーバの状態を持つ構造体。
///
/// `client` はサーバからエディタへ「話しかける」ための窓口
/// （ログ送信・通知・診断のプッシュなどに使う）。M1 以降はここに
/// 索引（`dbrain-core` のグラフ）を保持するフィールドが増えていく。
#[derive(Debug)]
struct Backend {
    client: Client,
}

// `#[tower_lsp::async_trait]` は、トレイトのメソッドを async にするための
// おまじない（Rust 標準ではトレイトの async メソッドに少し制約があるため）。
#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// 握手。クライアントが最初に一度だけ呼ぶ。
    ///
    /// サーバは「自分は何者で、どんな機能(capabilities)を提供できるか」を
    /// 返す。ここで宣言した機能だけがエディタから呼ばれるようになる。
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            // サーバ名とバージョン（バージョンはビルド時に Cargo.toml から取る）。
            server_info: Some(ServerInfo {
                name: "developer-brain-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // 文書の変更を「差分(INCREMENTAL)」で受け取る宣言。
                // 全文ではなく変わった部分だけ届くので、大きなファイルでも軽い。
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                // M2 でここに completion_provider / document_link_provider /
                // hover_provider / code_lens_provider を追加する。
                ..Default::default()
            },
        })
    }

    /// 握手が完了した直後に呼ばれる通知。ログを 1 行出すだけ。
    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "developer-brain-lsp initialized")
            .await;
    }

    /// 終了要求。後片付けがあればここで行う（今は何もない）。
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

// `#[tokio::main]` は「async な main を動かすための非同期ランタイム」を
// 用意するおまじない。LSP は本質的に「メッセージを待っては応える」処理なので
// 非同期と相性が良い。
#[tokio::main]
async fn main() {
    // エディタとは標準入出力でやり取りする。
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // LspService がプロトコル処理を担い、Backend が実際の応答を書く。
    let (service, socket) = LspService::new(|client| Backend { client });

    // サーバを起動し、エディタが切断するまでメッセージを捌き続ける。
    Server::new(stdin, stdout, socket).serve(service).await;
}
