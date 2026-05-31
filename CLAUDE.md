# CLAUDE.md

このファイルは Claude Code が毎回読み込むプロジェクト指示書です。**作業前に必ず守る前提**と、**進め方**を書いています。詳しい設計の背景は `docs/DESIGN.md`、公開仕様は各 `///` doc コメント（→ `cargo doc`）を参照してください。

## プロジェクト概要

Developer Brain は、VS Code 上で Markdown ドキュメント・ソースコード・用語集を **コードを理解する知識グラフ** として結ぶナレッジツール。主役は Rust 製のヘッドレスなコア（`dbrain-core`）で、VS Code 拡張・将来の MCP サーバ・CLI はすべてそのクライアント。

第一目標は **(1) 省メモリ** と **(2) AI（エージェント）フレンドリー**。この 2 つに反する設計判断はしない。

## 絶対に守る設計不変条件（NON-NEGOTIABLE）

これらを破る変更は提案しないこと。やむを得ず触れる場合は、まず理由を説明し承認を得る。

1. **参照は座標ではなくクエリ。行番号を保存しない。** リンクはシンボルの同一性（`SymbolDescriptor`）だけを持ち、位置は索引時に tree-sitter で都度解決する。
2. **コアは headless に保つ。** エディタ/LSP/特定プロトコルへの依存を `core` に持ち込まない。賢さは `core`、`lsp`/`mcp`/`cli` は薄いアダプタ。
3. **`References` 辺はシンボル間粒度で 1 本。** 個々の使用箇所の座標は保存せず、必要時に live で算出する（汚いコードでも索引を膨らませない）。
4. **重い値は `SymbolId` で intern する。** メモリ最優先。グラフの辺は ID で持ち、記述子はシンボル表に一元化。
5. **外部への出力は機械可読 JSON。** 人間向けログは stderr、機械可読データは stdout。

## アーキテクチャ（クレート）

| クレート | 役割 |
| :-- | :-- |
| `core` (`dbrain-core`) | 知識エンジン本体。エディタにもプロトコルにも非依存 |
| `lsp` (`dbrain-lsp`) | tower-lsp サーバ。LSP 境界の薄いアダプタ |
| `cli` (`dbrain`) | ヘッドレス検証・CI・AI 連携の入口 |
| `editor` | VS Code 拡張（TypeScript）。LSP クライアント＋グラフ Webview |

参照シンタックス: ドキュメント `[[path.md#見出し]]` / コード `[[path.rs@Mod::Type::sym]]`。区切りは コード `@` / スコープ `::` / 見出し `#`。用語集は本文を書き換えず仮想リンク（DocumentLink+Hover）。

## コーディング規約

- **Rust**: edition 2021。`rustfmt` 整形必須、`clippy -D warnings` を通す。ロジック（パーサ等）にはユニットテストを付ける。
- **doc コメントは公開仕様書**。`///` `//!` は `cargo doc` で HTML 化され Pages に公開される。公開 API や仕様を変えたら **必ず doc コメントを更新**。`ci.yml` の docs ジョブが壊れたリンクを検出する。
- **コメントは豊富に、初学者にも分かるように**（「何を」だけでなく「なぜ」と具体例）。これは本プロジェクトの明示方針。
- **TypeScript**: `strict`。ビルドは esbuild。

## 作業フロー（毎回）

1. 着手前に、その変更がどの不変条件・どのマイルストーンに関わるかを一言述べる。
2. **小さく分けて進める**。1 ステップ＝レビュー可能な単位。一気に大量変更しない。
3. 設計の分岐点では勝手に決めず、選択肢と推奨を示して確認を取る。
4. 実装したら `/preflight`（下記）でローカル検査を通してからコミット提案する。
5. コミットメッセージは何を・なぜを簡潔に。関連 Issue を紐づける。

## 進め方の好み（学習重視）

ユーザーは本プロジェクトを学習の機会にしている。**実装の前に設計意図を説明**し、新しい概念（tree-sitter、LSP、グラフアルゴリズム等）が出たら噛み砕く。理解の確認を挟みながらマイルストーン単位で進める。冗長な賞賛は不要、率直で建設的に。

## モデル運用（利用量の節約）

利用量を抑えるため、タスクの種類でモデルを使い分ける。切り替えは `/model`、現在のモデルは `/status` で確認できる。

- **Opus** … 設計・アーキテクチャ・難所のデバッグ・トレードオフ判断。
- **Sonnet** … 通常の実装・リファクタ・テスト記述。
- **Haiku** … 機械的作業（検索・整形・定型編集・要約・説明・検査の実行）。

Claude は、タスクの境目で「ここは機械的なので安価なモデルに落とせる」と判断したら、進める前に `/model` での切り替えを **提案** すること。逆に、判断を要する設計作業で安易にモデルを下げない。

機械的な小作業は、安価なモデルで動く **サブエージェント** に委譲して本体（Opus 等）のトークンを温存する:

- `checks-runner`（Haiku）… ローカル検査の実行と報告。
- `explorer`（Haiku）… 読み取り専用のコード調査。

サブエージェントを一律で安価に固定したい場合は、環境変数 `CLAUDE_CODE_SUBAGENT_MODEL`（例: `claude-haiku-4-5-20251001`）を設定する。これは frontmatter の `model:` 指定より優先され、コスト上限として効く。

## ビルドと検査

```sh
cargo build
cargo test --all
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace   # Pages と同じ内容
cd editor && npm run build                                   # 拡張（型チェック+bundle）
```

LSP を拡張から使う開発時: `DBRAIN_LSP_PATH=./target/debug/dbrain-lsp` を指定。

## 現在地と次の一手

- **完了: M0**（足場・データモデル確定・参照パーサ・CI/CD・Pages・本ファイル群）。
- **次: M1（コア本体）**。サブタスク:
  1. tree-sitter の言語クレート導入（まず `c` / `cpp` / `rust` / `python`）。
  2. 定義抽出（tags クエリ）＋ **囲みスコープを辿った修飾名の再構成**で `SymbolEntry` 表を作る。
  3. `pulldown-cmark` で md を走査し `[[]]` と見出しを抽出。
  4. グラフ構築（`petgraph` 想定）と `Reference` 解決。
  5. `dbrain index <dir>` で機械可読グラフ JSON を出力（headless 完結）。

ロードマップ全体: M0 足場 → M1 コア → M2 リンク体験(LSP) → M3 グラフビュー(Webview) → M4 増分更新+監視 → M5+ 陳腐化検出・用語集Linter・MCP。

## 参照

- 設計判断の背景と根拠: `docs/DESIGN.md`
- アーキテクチャ/ビルド/CI-CD: `README.md`
- 公開仕様（型）: `core/src/lib.rs` と `core/src/model.rs` の doc コメント
