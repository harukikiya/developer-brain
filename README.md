# Developer Brain

VS Code 上で、Markdown ドキュメント・ソースコード・用語集を、**コードを理解する知識グラフ**として結ぶナレッジ管理ツール。ドキュメントの陳腐化を防ぎ、人間にもエージェントにも問い合わせ可能なグラフを提供する。

## 設計の核心

**参照は「座標」ではなく「クエリ」。** リンクにはシンボルの同一性（記述子）だけを持たせ、現在位置は索引時に tree-sitter で都度解決する。行番号は一切保存しない。これにより、関数や変数が行を跨いで移動・リファクタされてもリンクは切れない。同じ仕組みが次の 3 つを同時に満たす:

- **人間** … 行ズレ・リファクタで切れない
- **エージェント** … 壊れない参照ハンドル（「`parse_frame` を参照する文書を直して」が成立する）
- **メモリ** … 位置を持たないので索引がコンパクト

## アーキテクチャ

主役は Rust コア（ヘッドレスな知識エンジン）。VS Code 拡張・MCP サーバ・CLI はいずれもそのクライアント。

```
クライアント:  VS Code 拡張(TS)   AI エージェント   CLI/CI
                    │ LSP             │ MCP           │
                    └─────────────────┴───────────────┘
                                 │
        ┌────────────────────────────────────────────┐
        │  dbrain-core（lib）= エンジン                  │
        │  索引 / グラフ / リンク解決 / 用語集 / JSON 出力  │
        └────────────────────────────────────────────┘
```

| クレート | 役割 |
| :-- | :-- |
| `core` (`dbrain-core`) | ヘッドレスな知識エンジン。エディタにもプロトコルにも非依存 |
| `lsp`  (`dbrain-lsp`)  | tower-lsp サーバ。補完/DocumentLink/Hover/CodeLens の境界アダプタ |
| `cli`  (`dbrain`)      | ヘッドレス検証・CI・将来のエージェント連携の入口 |
| `editor`               | VS Code 拡張（TS）。LSP クライアント＋グラフ Webview |

## 参照シンタックス

| 形式 | 例 | 解決先 |
| :-- | :-- | :-- |
| ドキュメント | `[[docs/spec.md]]` / `[[docs/spec.md#見出し]]` / `[[短縮名]]` | Document / Section |
| コードシンボル | `[[lin/protocol.rs@LinFrame::parse]]` | CodeSymbol |

区切り文字: コード `@` / スコープ `::` / 見出し `#`。用語集リンクは構文を持たず、出現箇所を DocumentLink + Hover で**仮想的に**リンク化する（本文は無改変）。

## マイルストーン

- **M0（このコミット）** 足場: workspace + tower-lsp 空サーバ + 拡張スケルトン + Dev Container + CI。コアに背骨の型と参照パーサ + テスト。
- **M1** コア: ファイル走査 → `[[]]` 抽出 + tree-sitter シンボル索引 → グラフ構築 → JSON 出力（headless 完結）。
- **M2** リンク体験: LSP で `[[` 補完・DocumentLink ジャンプ・Hover。
- **M3** グラフビュー: Webview（Cytoscape）でノード/エッジ描画、クリックでファイルを開く。
- **M4** 増分更新 + ファイル監視（別スレッドで非ブロック）。
- **M5+** 陳腐化検出・用語集 Linter・MCP アダプタ。

## ビルドと実行（M0）

```sh
# Rust コア + サーバ + CLI
cargo build
cargo test --all

# LSP サーバ単体（stdio）
./target/debug/dbrain-lsp

# CLI（M1 までは空グラフを返すプレースホルダ）
./target/debug/dbrain index .

# VS Code 拡張
cd editor
npm install
npm run build   # 型チェック + esbuild バンドル
```

拡張から開発中の LSP サーバを使うには、環境変数 `DBRAIN_LSP_PATH` にビルド済みバイナリのパスを指定して VS Code を起動する。

## ライセンス

MIT OR Apache-2.0

## CI/CD と Pages 公開

`.github/` に以下を用意してある。

| ファイル | 役割 | 実行タイミング |
| :-- | :-- | :-- |
| `workflows/ci.yml` | fmt / clippy / test / rustdoc 検証 / 拡張ビルド | push・PR ごと |
| `workflows/pages.yml` | rustdoc を GitHub Pages へ公開 | `main` への push |
| `dependabot.yml` | cargo / npm / actions の更新 PR を自動作成 | 毎週 |
| `ISSUE_TEMPLATE/`, `pull_request_template.md` | Issue / PR の雛形 | 起票時 |

**doc コメント = 公開仕様書** という方針。`///` `//!` で書いたコメントが
`cargo doc` で HTML になり、Pages にそのまま公開される。仕様を変えるときは
コメントを直す。`ci.yml` の `docs` ジョブが「壊れたドキュメントリンク」を
警告ゼロで検出するので、仕様の腐敗を防げる。

### Pages を有効化する（リポジトリで 1 回だけ）

1. リポジトリの **Settings → Pages** を開く。
2. **Build and deployment → Source** を **GitHub Actions** にする。
3. `main` に push すると `pages.yml` が走り、次の URL に公開される:
   `https://<ユーザ名>.github.io/<リポジトリ名>/dbrain_core/index.html`
   （トップ `index.html` はコアの doc へ自動転送する）

### 将来足すと良い CD（M3 以降）

- **リリース**: タグを打つと OS 別の `dbrain-lsp` バイナリをビルドし、
  VS Code 拡張(`.vsix`)に同梱して Releases に添付するワークフロー。
- **拡張の公開**: VS Code Marketplace / Open VSX への自動 publish。
- **mdBook**: 概念的な解説（チュートリアル等）を narrative で書きたくなったら、
  rustdoc と並べて Pages に同居させる。
