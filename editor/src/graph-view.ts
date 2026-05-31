// =============================================================================
// Developer Brain — グラフビュー（Webview 内スクリプト）
// =============================================================================
//
// このファイルは VS Code Webview の中で動く。通常の Web ページとほぼ同じだが、
// VS Code との通信は window.addEventListener("message", ...) で行う。
//
// データの流れ:
//   extension.ts → postMessage({type:"graph", data: GraphJson})
//     → ここで受け取り → Cytoscape でレンダリング
//
// 3-2: まず <pre> にデータをそのまま表示して配管を確認する（描画は 3-3）。
// 3-3: Cytoscape.js でグラフを描く。
// 3-4: クリックジャンプ + ノード種別フィルタ。
// =============================================================================

import cytoscape from "cytoscape";

// VS Code の Webview API（postMessage / setState 等）。
// `acquireVsCodeApi` は Webview 環境でのみ存在するグローバル関数。
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
// メッセージ受信と描画
// --------------------------------------------------------------------------

const vscode = acquireVsCodeApi();

// グラフが届くまでローディング表示。
const container = document.getElementById("cy");
const status = document.getElementById("status");

window.addEventListener("message", (event: MessageEvent) => {
  const msg = event.data as { type: string; data: GraphJson };
  if (msg.type !== "graph") return;

  const graph = msg.data;

  if (status) {
    status.textContent = `${graph.nodes.length} ノード / ${graph.edges.length} 辺`;
  }

  renderGraph(graph);
});

// --------------------------------------------------------------------------
// Cytoscape レンダリング（3-3）
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

  // 既存のインスタンスがあれば破棄して作り直す（再描画）。
  container.innerHTML = "";

  cytoscape({
    container,

    // ノードと辺のデータ。Cytoscape は {data: {...}} 形式で受け取る。
    elements: [
      ...graph.nodes.map((n) => ({
        data: { id: n.id, label: n.label, kind: n.kind },
      })),
      ...graph.edges.map((e, i) => ({
        data: {
          id: `edge-${i}`,
          source: e.from,
          target: e.to,
          kind: e.kind,
        },
      })),
    ],

    // スタイル定義。
    style: [
      {
        selector: "node",
        style: {
          label: "data(label)",
          // ノード種別で色分けする。
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
      // ホバー・選択時のハイライト。
      {
        selector: "node:selected",
        style: { "border-width": 3, "border-color": "#fff" },
      },
    ],

    // Cola / cose 等のレイアウト。section は親 doc のそばに置きたいが、
    // シンプルな `cose`（force-directed）で十分見やすい。
    layout: {
      name: "cose",
      animate: false,
      nodeRepulsion: () => 4096,
      padding: 20,
    },
  }).on("tap", "node", (evt: cytoscape.EventObject) => {
    // ノードをクリックしたら extension.ts へ通知する（3-4: 定義ジャンプ）。
    const node = evt.target as cytoscape.NodeSingular;
    vscode.postMessage({ type: "nodeClick", id: node.id(), kind: node.data("kind") });
  });
}
