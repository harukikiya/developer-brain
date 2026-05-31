// =============================================================================
// Developer Brain — VS Code 拡張のエントリポイント
// =============================================================================
//
// この拡張の仕事は大きく 2 つ:
//   1. Rust 製の言語サーバ (dbrain-lsp) を起動し VS Code と橋渡しする。
//   2. "Developer Brain: Show Graph" コマンドで Webview グラフビューを表示する。
//
// 賢い処理（索引・リンク解決・グラフ構築）は全部サーバ側にある。
// この拡張は「薄いクライアント」として、受け取ったデータを表示するだけ。

import * as path from "path";
import {
  ExtensionContext,
  Uri,
  ViewColumn,
  WebviewPanel,
  commands,
  window,
  workspace,
} from "vscode";
import {
  ExecuteCommandRequest,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

// 既に開いているグラフパネルを再利用する（複数開かないようにする）。
let graphPanel: WebviewPanel | undefined;

/**
 * 拡張が有効化されたときに呼ばれる。
 */
export function activate(context: ExtensionContext): void {
  // --- 1. LSP サーバの起動 ---
  const serverPath =
    process.env.DBRAIN_LSP_PATH ??
    context.asAbsolutePath(path.join("..", "target", "debug", "dbrain-lsp"));

  const serverOptions: ServerOptions = {
    run: { command: serverPath, transport: TransportKind.stdio },
    debug: { command: serverPath, transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "markdown" },
      { scheme: "file", language: "rust" },
      { scheme: "file", language: "c" },
      { scheme: "file", language: "cpp" },
      { scheme: "file", language: "python" },
    ],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher(
        "**/*.{md,rs,c,h,cpp,hpp,py}"
      ),
    },
  };

  client = new LanguageClient(
    "developerBrain",
    "Developer Brain",
    serverOptions,
    clientOptions
  );
  void client.start();

  // --- 2. "Show Graph" コマンドの登録（3-2/3-3/3-4）---
  context.subscriptions.push(
    commands.registerCommand("developerBrain.showGraph", () => {
      void showGraphView(context);
    })
  );
}

/**
 * グラフビューを開く（or 既存パネルを前面に出す）。
 *
 * 流れ:
 *   1. Webview パネルを作成（or 再利用）
 *   2. LSP サーバに "dbrain/graph" コマンドを送り、グラフ JSON を受け取る
 *   3. Webview に postMessage で JSON を渡す
 *   4. Webview 内の graph-view.js が受け取って Cytoscape で描画する
 */
async function showGraphView(context: ExtensionContext): Promise<void> {
  // 既存パネルがあれば前面に出す。
  if (graphPanel) {
    graphPanel.reveal(ViewColumn.Beside);
  } else {
    graphPanel = window.createWebviewPanel(
      "developerBrainGraph", // 内部 ID（パネルの種類を識別する）
      "Developer Brain: Graph", // タブに表示されるタイトル
      ViewColumn.Beside, // 現在のエディタの隣に表示
      {
        enableScripts: true, // Webview 内で JS を動かすために必須
        // dist/graph-view.js をロードできるよう、dist/ フォルダへのアクセスを許可する。
        localResourceRoots: [Uri.joinPath(context.extensionUri, "dist")],
        retainContextWhenHidden: true, // 隠れていても状態を保持（再描画を防ぐ）
      }
    );

    // パネルが閉じられたらインスタンスを破棄する。
    graphPanel.onDidDispose(() => {
      graphPanel = undefined;
    });

    // Webview から届くメッセージを処理する（3-4: ノードクリック）。
    graphPanel.webview.onDidReceiveMessage(
      async (msg: { type: string; id: string; kind: string }) => {
        if (msg.type === "nodeClick") {
          await handleNodeClick(msg.id, msg.kind);
        }
      }
    );

    // graph-view.js の Webview URI（拡張のローカルファイルを Webview から読むための変換）。
    const scriptUri = graphPanel.webview.asWebviewUri(
      Uri.joinPath(context.extensionUri, "dist", "graph-view.js")
    );

    graphPanel.webview.html = buildWebviewHtml(scriptUri);
  }

  // LSP サーバに問い合わせてグラフ JSON を取得する。
  // まだ索引が完成していなければ空グラフが返る。
  let graphData: unknown = { nodes: [], edges: [] };
  if (client) {
    try {
      graphData = await client.sendRequest(ExecuteCommandRequest.type, {
        command: "dbrain/graph",
        arguments: [],
      });
    } catch {
      void window.showWarningMessage(
        "Developer Brain: グラフの取得に失敗しました。LSP サーバが起動中か確認してください。"
      );
    }
  }

  // Webview に JSON を送る。
  void graphPanel?.webview.postMessage({ type: "graph", data: graphData });
}

/**
 * グラフビューの Webview HTML を組み立てる。
 *
 * Content Security Policy (CSP) について:
 *   VS Code Webview はデフォルトで外部リソースを全てブロックする。
 *   `script-src` に `${webview.cspSource}` を指定すると、
 *   `asWebviewUri()` で変換したローカル URI だけを許可できる。
 *   nonce を使う方法もあるが、固定スクリプトのみなのでここでは省略している。
 */
function buildWebviewHtml(scriptUri: Uri): string {
  return /* html */ `<!DOCTYPE html>
<html lang="ja">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Developer Brain: Graph</title>
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      background: var(--vscode-editor-background);
      color: var(--vscode-editor-foreground);
      font-family: var(--vscode-font-family);
      display: flex;
      flex-direction: column;
      height: 100vh;
      overflow: hidden;
    }
    #toolbar {
      display: flex;
      align-items: center;
      gap: 12px;
      padding: 6px 12px;
      background: var(--vscode-sideBar-background);
      border-bottom: 1px solid var(--vscode-panel-border);
      flex-shrink: 0;
      flex-wrap: wrap;
    }
    #status {
      font-size: 12px;
      color: var(--vscode-descriptionForeground);
      margin-left: auto;
    }
    .filter-group { display: flex; align-items: center; gap: 6px; font-size: 12px; }
    .filter-group label { display: flex; align-items: center; gap: 4px; cursor: pointer; }
    .dot {
      width: 10px; height: 10px; border-radius: 50%; display: inline-block;
    }
    #cy {
      flex: 1;
      width: 100%;
    }
    #loading {
      position: absolute;
      top: 50%;
      left: 50%;
      transform: translate(-50%, -50%);
      font-size: 14px;
      color: var(--vscode-descriptionForeground);
    }
  </style>
</head>
<body>
  <div id="toolbar">
    <span style="font-weight:600;font-size:13px;">Knowledge Graph</span>
    <div class="filter-group">
      <label><input type="checkbox" data-kind="doc"     checked> <span class="dot" style="background:#4A90D9"></span> doc</label>
      <label><input type="checkbox" data-kind="section" checked> <span class="dot" style="background:#7EC8E3"></span> section</label>
      <label><input type="checkbox" data-kind="symbol"  checked> <span class="dot" style="background:#E8824A"></span> symbol</label>
      <label><input type="checkbox" data-kind="term"    checked> <span class="dot" style="background:#7DBD77"></span> term</label>
    </div>
    <span id="status">読み込み中…</span>
  </div>
  <div id="cy"></div>
  <script src="${scriptUri}"></script>
</body>
</html>`;
}

/**
 * グラフ上のノードがクリックされたときに定義ジャンプを行う（3-4）。
 *
 * NodeId の文字列フォーマット（core/src/index.rs の node_id_string より）:
 *   - doc:path/to/file.md
 *   - section:path/to/file.md#見出し
 *   - symbol:42  （SymbolId の数値）
 */
async function handleNodeClick(nodeId: string, kind: string): Promise<void> {
  const rootUri = workspace.workspaceFolders?.[0]?.uri;
  if (!rootUri) return;

  if (kind === "doc") {
    // "doc:path/to/file.md" → ファイルを開く
    const rel = nodeId.replace(/^doc:/, "");
    const fileUri = Uri.joinPath(rootUri, rel);
    await commands.executeCommand("vscode.open", fileUri);
  } else if (kind === "section") {
    // "section:path/to/file.md#見出し" → ファイルを開く（見出し位置は M4 以降）
    const rel = nodeId.replace(/^section:/, "").replace(/#.*$/, "");
    const fileUri = Uri.joinPath(rootUri, rel);
    await commands.executeCommand("vscode.open", fileUri);
  } else if (kind === "symbol") {
    // symbol ノードのジャンプは LSP の GotoDefinition に委ねる（M3 スコープ外）。
    // 今後 SymbolId → ファイル位置のマッピングをコマンド経由で取得する設計を検討。
    void window.showInformationMessage(
      `symbol ノード「${nodeId}」のジャンプは今後実装予定です。`
    );
  }
}

/**
 * 拡張が無効化される時に呼ばれる。
 */
export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
