// =============================================================================
// Developer Brain — グラフビュー（Webview 内スクリプト）
// =============================================================================
//
// VS Code Webview の中で動く。extension.ts との通信は postMessage / onmessage。
//
// データの流れ（受信）:
//   extension.ts → postMessage({type:"graph", data: GraphJson}) → renderGraph()
//
// データの流れ（送信）:
//   ノードクリック → postMessage({type:"nodeClick", id, kind})
//   更新ボタン    → postMessage({type:"refresh"})
// =============================================================================

import cytoscape from "cytoscape";

declare function acquireVsCodeApi(): {
  postMessage(message: unknown): void;
  setState(state: unknown): void;
  getState(): unknown;
};

// --------------------------------------------------------------------------
// 型定義（core の GraphJson スキーマに対応）
// --------------------------------------------------------------------------

interface GraphNode {
  id: string;
  kind: "doc" | "section" | "symbol" | "term";
  label: string;
}

interface GraphEdge {
  kind: "links" | "references" | "mentions" | "contains";
  from: string;
  to: string;
}

interface GraphJson {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

// --------------------------------------------------------------------------
// 初期化
// --------------------------------------------------------------------------

const vscode = acquireVsCodeApi();
const container = document.getElementById("cy");
const statusEl = document.getElementById("status");

// レンダリング後にフィルタや Fit ボタンがアクセスできるよう
// モジュールスコープに保持する。
let cy: cytoscape.Core | null = null;

// --------------------------------------------------------------------------
// メッセージ受信
// --------------------------------------------------------------------------

window.addEventListener("message", (event: MessageEvent) => {
  const msg = event.data as { type: string; data: GraphJson };
  if (msg.type !== "graph") return;

  const graph = msg.data;
  if (statusEl) {
    statusEl.textContent = `${graph.nodes.length} ノード / ${graph.edges.length} 辺`;
  }
  renderGraph(graph);
});

// --------------------------------------------------------------------------
// Cytoscape レンダリング
// --------------------------------------------------------------------------

/** ノード種別に対応する背景色 */
const NODE_COLORS: Record<GraphNode["kind"], string> = {
  doc: "#4A90D9",
  section: "#7EC8E3",
  symbol: "#E8824A",
  term: "#7DBD77",
};

function renderGraph(graph: GraphJson): void {
  if (!container) return;

  // 既存インスタンスを破棄してから作り直す（更新時の二重描画を防ぐ）。
  if (cy) {
    cy.destroy();
    cy = null;
  }
  container.innerHTML = "";

  cy = cytoscape({
    container,
    elements: [
      ...graph.nodes.map((n) => ({
        data: { id: n.id, label: n.label, kind: n.kind },
      })),
      ...graph.edges.map((e, i) => ({
        data: { id: `edge-${i}`, source: e.from, target: e.to, kind: e.kind },
      })),
    ],
    style: [
      {
        selector: "node",
        style: {
          label: "data(label)",
          "background-color": (ele: cytoscape.NodeSingular) =>
            NODE_COLORS[ele.data("kind") as GraphNode["kind"]] ?? "#aaa",
          color: "#fff",
          "text-valign": "center",
          "text-halign": "center",
          "font-size": "10px",
          width: 60,
          height: 60,
          "text-wrap": "wrap",
          "text-max-width": "55px",
        },
      },
      {
        selector: "node[kind='section']",
        style: { width: 40, height: 40, "font-size": "9px" },
      },
      {
        selector: "edge",
        style: {
          width: 1.5,
          "line-color": "#999",
          "target-arrow-color": "#999",
          "target-arrow-shape": "triangle",
          "curve-style": "bezier",
        },
      },
      {
        selector: "edge[kind='contains']",
        style: { "line-style": "dashed", width: 1, "line-color": "#ccc" },
      },
      {
        selector: "node:selected",
        style: { "border-width": 3, "border-color": "#fff" },
      },
    ],
    layout: {
      name: "cose",
      animate: false,       // 大きいグラフでもアニメなしで即表示
      nodeRepulsion: () => 4096,
      padding: 20,
    },
    // ズームアウト時に辺を省略して描画負荷を下げる。
    // ノードが多いワークスペースで有効。
    hideEdgesOnViewport: true,
    textureOnViewport: true,
  });

  cy.on("tap", "node", (evt: cytoscape.EventObject) => {
    const node = evt.target as cytoscape.NodeSingular;
    vscode.postMessage({ type: "nodeClick", id: node.id(), kind: node.data("kind") });
  });

  // レイアウト完了後にデフォルトフィルタを適用してビューに収める。
  applyVisibility(cy);
  cy.fit(undefined, 20);

  // コントロールを初回のみ接続する（再描画のたびに重複登録しないよう once フラグ管理）。
  if (!controlsAttached) {
    attachControls();
    controlsAttached = true;
  }
}

// --------------------------------------------------------------------------
// フィルタ（表示/非表示の管理）
// --------------------------------------------------------------------------

// @types/cytoscape の特定バージョンでは NodeCollection / EdgeCollection 上の
// show() / hide() / hidden() の型定義が欠落しているため、
// 中間型 Visible にキャストして呼び出す。実行時の動作は正常。
type Visible = { show(): void; hide(): void; hidden(): boolean };
const vis = (x: unknown): Visible => x as Visible;

/**
 * ツールバーのチェックボックス状態を読んで、ノードと辺の表示を更新する。
 *
 * 手順:
 *   1. 全要素を一旦表示（show）
 *   2. 非表示ノード種別のノードを hide
 *   3. 非表示辺種別の辺を hide
 *   4. 片端が非表示ノードである辺も hide（宙吊り辺を消す）
 */
function applyVisibility(instance: cytoscape.Core): void {
  vis(instance.elements()).show();

  // ノード種別フィルタ
  document
    .querySelectorAll<HTMLInputElement>("input[data-kind]")
    .forEach((cb) => {
      if (!cb.checked) {
        vis(instance.nodes(`[kind = "${cb.dataset.kind}"]`)).hide();
      }
    });

  // 辺種別フィルタ
  document
    .querySelectorAll<HTMLInputElement>("input[data-edge-kind]")
    .forEach((cb) => {
      if (!cb.checked) {
        vis(instance.edges(`[kind = "${cb.dataset.edgeKind}"]`)).hide();
      }
    });

  // 片端が非表示のノードである辺は自動的には消えないので明示的に hide する。
  instance.edges().forEach((edge) => {
    if (vis(edge.source()).hidden() || vis(edge.target()).hidden()) {
      vis(edge).hide();
    }
  });
}

// --------------------------------------------------------------------------
// コントロール（ボタン + チェックボックス）のイベント接続
// --------------------------------------------------------------------------

let controlsAttached = false;

function attachControls(): void {
  // section 以外のノード種別 + 辺種別チェックボックス → client-side フィルタ
  //（再取得不要。状態はメモリ上に保持されているため即座に反映できる）
  document
    .querySelectorAll<HTMLInputElement>(
      "input[data-kind]:not([data-kind='section']), input[data-edge-kind]"
    )
    .forEach((cb) => {
      cb.addEventListener("change", () => {
        if (cy) applyVisibility(cy);
      });
    });

  // section チェックボックスは特別扱い。
  //   ON  → sections を含むグラフを再取得（デフォルトでは送られてこないため）。
  //   OFF → client-side hide で済ませる（ラウンドトリップなし）。
  const sectionCb = document.querySelector<HTMLInputElement>(
    "input[data-kind='section']"
  );
  sectionCb?.addEventListener("change", () => {
    if (sectionCb.checked) {
      if (statusEl) statusEl.textContent = "セクション読み込み中…";
      vscode.postMessage({ type: "sectionChange", include: true });
    } else {
      if (cy) applyVisibility(cy);
    }
  });

  // Fit ボタン → グラフ全体をビューに収める
  document.getElementById("fit-btn")?.addEventListener("click", () => {
    cy?.fit(undefined, 20);
  });

  // 更新ボタン → section の現在状態を引き継いで extension.ts に再取得を依頼する。
  // section が ON のまま更新しても sections 込みのグラフが返る。
  document.getElementById("refresh-btn")?.addEventListener("click", () => {
    if (statusEl) statusEl.textContent = "更新中…";
    const includeSections = sectionCb?.checked ?? false;
    vscode.postMessage({ type: "refresh", includeSections });
  });
}
