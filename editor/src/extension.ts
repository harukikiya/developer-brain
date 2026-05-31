// =============================================================================
// Developer Brain — VS Code 拡張のエントリポイント
// =============================================================================
//
// この拡張の仕事は驚くほど少なく、「Rust 製の言語サーバ(dbrain-lsp)を起動し、
// VS Code と橋渡しする」だけです。賢い処理は全部サーバ側にあります。
//
// VS Code 拡張のライフサイクル:
//   - activate()   … 拡張が有効化された時に 1 回呼ばれる（ここで起動処理）
//   - deactivate() … 無効化/終了時に呼ばれる（ここで後片付け）
// =============================================================================

import * as path from "path";
import { ExtensionContext, commands, window, workspace } from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

// 言語クライアント（サーバとの接続）を、後で deactivate でも触れるよう
// モジュールスコープに保持しておく。
let client: LanguageClient | undefined;

/**
 * 拡張が有効化されたときに呼ばれる。
 * @param context 拡張のリソース管理用。subscriptions に登録した物は
 *                終了時に自動で破棄される（後始末を VS Code に任せられる）。
 */
export function activate(context: ExtensionContext): void {
  // --- 1. Rust 製 LSP サーバの実行ファイルの場所を決める ---
  // 開発中は環境変数 DBRAIN_LSP_PATH で `target/debug/dbrain-lsp` を指す。
  // 将来の配布版では、OS 別にビルドしたバイナリを拡張に同梱して使う。
  const serverPath =
    process.env.DBRAIN_LSP_PATH ??
    context.asAbsolutePath(path.join("..", "target", "debug", "dbrain-lsp"));

  // --- 2. サーバの起動方法を指定する ---
  // run = 通常起動, debug = デバッグ起動。どちらも stdio で通信する。
  const serverOptions: ServerOptions = {
    run: { command: serverPath, transport: TransportKind.stdio },
    debug: { command: serverPath, transport: TransportKind.stdio },
  };

  // --- 3. クライアント側の設定 ---
  const clientOptions: LanguageClientOptions = {
    // どの種類のファイルでこのサーバを働かせるか。
    // 知識グラフは「ドキュメント(md)」と「コード」をまたぐので両方を対象にする。
    documentSelector: [
      { scheme: "file", language: "markdown" },
      { scheme: "file", language: "rust" },
      { scheme: "file", language: "c" },
      { scheme: "file", language: "cpp" },
      { scheme: "file", language: "python" },
    ],
    // 関連ファイルが作成/変更/削除されたらサーバへ知らせる。
    // M4 の「ファイル監視による増分更新」の土台になる。
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher(
        "**/*.{md,rs,c,h,cpp,hpp,py}"
      ),
    },
  };

  // --- 4. クライアントを生成して起動 ---
  client = new LanguageClient(
    "developerBrain", // 内部 ID
    "Developer Brain", // 表示名
    serverOptions,
    clientOptions
  );
  void client.start(); // 起動は非同期。完了を待つ必要はないので void で明示。

  // --- 5. コマンドを登録 ---
  // コマンドパレットの "Developer Brain: Show Graph" がこれを呼ぶ。
  context.subscriptions.push(
    commands.registerCommand("developerBrain.showGraph", () => {
      // M3 で Webview のグラフビュー（Cytoscape）を実装する。
      void window.showInformationMessage(
        "Developer Brain: グラフビューは M3 で実装予定です。"
      );
    })
  );
}

/**
 * 拡張が無効化される時に呼ばれる。
 * クライアント（＝サーバ接続）を止めて後片付けする。
 */
export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
