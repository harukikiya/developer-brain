//! # データモデル（このツールの「背骨」）
//!
//! ここで定義する型が、Developer Brain 全体の土台です。VS Code 拡張も
//! MCP サーバも CLI も、最終的にはここの型を読み書きします。なので、
//! ここの設計が良ければ全部が素直になり、ここが歪むと全部が歪みます。
//!
//! ## 設計の核心: 「参照は座標ではなくクエリ」
//!
//! ドキュメントからコードへリンクを張るとき、素朴にやると
//! 「`protocol.rs` の 42 行目」のように **位置（座標）** を保存します。
//! しかしコードは編集で動くので、関数が 42 行目から 87 行目に移れば
//! リンクは即座に壊れます。
//!
//! そこでこのツールは位置を保存しません。代わりに
//! **シンボルの同一性**（「`protocol.rs` の中の `LinFrame::parse` という関数」）
//! だけを [`SymbolDescriptor`] として保存し、実際の現在位置は
//! 索引のたびに tree-sitter（コードを構文解析するライブラリ）で
//! 解決し直します。これを「参照は座標ではなくクエリ」と呼んでいます。
//!
//! この一つの方針が、3 つの利点を同時に生みます:
//!
//! - **人間** … 行がズレてもリファクタしてもリンクが切れない
//! - **エージェント(AI)** … 壊れない参照ハンドルが手に入る
//!   （「`parse_frame` を参照している文書を直して」が成立する）
//! - **メモリ** … 位置を持たないぶん索引が軽い

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// パス・名前まわりの基本型
// ---------------------------------------------------------------------------

/// ワークスペース相対パス。常に `/` 区切りに正規化して保持する。
///
/// Windows と macOS/Linux で区切り文字が違う（`\` と `/`）ので、
/// 内部表現は `/` に統一しておく。こうしておくと、別 OS で開いても
/// リンクが一致する。
///
/// `pub String` にしているのは M0 の簡潔さ優先。将来パスの不変条件
/// （絶対パスを弾く等）を強制したくなったら、フィールドを private にして
/// コンストラクタ経由に変えればよい。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelPath(pub String);

/// スコープを辿ったシンボル名を、外側から内側への順で持つ。
///
/// 例:
/// - Rust/C++ の `LinFrame::parse` → `["LinFrame", "parse"]`（2 段）
/// - C の `lin_parse_frame` → `["lin_parse_frame"]`（C は入れ子が無いので 1 段）
///
/// つまり「スコープの深さ」が要素数に対応する。C のように名前空間や
/// クラスを持たない言語では、ほぼ常に要素 1 個になる。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolPath(pub Vec<String>);

/// シンボルの種別。何の定義かを表す。
///
/// 必須ではなく「補助情報」。リンク解決のときに候補が複数あった場合の
/// 絞り込みや、グラフビューでのアイコン/色分け、Hover 表示に使う。
///
/// `#[serde(rename_all = "snake_case")]` を付けているので、JSON では
/// `"function"` `"struct"` のように小文字スネークケースで出力される
/// （Rust の慣習 `Function` ではなく、JSON/外部連携で扱いやすい形にする）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Type,
    Const,
    Static,
    Field,
    /// enum のバリアント（Rust の `Enum::Variant`）。C の列挙子は意味が異なるため Const を使う。
    Variant,
    Variable,
    Macro,
    Module,
}

// ---------------------------------------------------------------------------
// 参照（リンク）の「解決前」の表現
// ---------------------------------------------------------------------------

/// コードシンボルの「同一性」。**位置情報は一切含まない**のが肝。
///
/// これが `[[file@Sym]]` というリンクに保存される全て。
/// 「どのファイルの、どのスコープの、何という名前のシンボルか」だけを持ち、
/// 「今それが何行目にあるか」は持たない。行は索引時に解決する。
///
/// # 例
/// `[[lin/protocol.rs@LinFrame::parse]]` は次の記述子になる:
/// ```text
/// SymbolDescriptor {
///     file: "lin/protocol.rs",
///     path: ["LinFrame", "parse"],
///     kind: None,           // 任意
///     disambiguator: None,  // 任意
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolDescriptor {
    /// どのファイルのシンボルか。
    ///
    /// C の `static` 関数のように、別ファイルで同名が共存し得る場合でも、
    /// この `file` があるおかげで `parser.c@reset` と `display.c@reset` は
    /// 最初から別物として区別できる。
    pub file: RelPath,
    /// スコープを辿った名前（[`SymbolPath`] を参照）。
    pub path: SymbolPath,
    /// 種別（任意）。曖昧性解消・表示の補助に使う。
    pub kind: Option<SymbolKind>,
    /// オーバーロード解決のヒント（任意）。
    ///
    /// C++ のように「同名・引数違い」の関数が複数あると、名前だけでは
    /// どれか決まらない。その場合でも **シグネチャ記述を必須にはしない** 方針。
    /// 普段は `@parse` と書き、本当に曖昧なときだけここにヒントを入れる。
    pub disambiguator: Option<String>,
}

/// ドキュメントリンクの宛先。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocTarget {
    /// 完全パス指定。例: `docs/spec.md`。
    Path(RelPath),
    /// 短縮名。例: `仕様書`。ワークスペース内で一意に定まる場合のみ解決する
    /// （Obsidian のノート名リンクに相当）。
    Name(String),
}

/// リンクテキストを解析した「解決前」の参照表現。
///
/// `[[ ... ]]` の中身をパースするとこの型になる。まだ「どこを指すか（位置）」
/// は決まっておらず、「何を指したいか（意図）」だけが入っている。
/// 実際の位置への変換は [`Resolution`] 側で行う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reference {
    /// ドキュメント/セクションへの参照。
    /// `[[note.md]]` / `[[note.md#見出し]]` / `[[名前]]`
    Doc {
        target: DocTarget,
        /// `#` の後ろの見出し名（任意）。
        heading: Option<String>,
    },
    /// コードシンボルへの参照。`[[file.rs@Mod::Type::sym]]`
    Code(SymbolDescriptor),
}

impl Reference {
    /// `[[ ... ]]` の **内側** 文字列を参照表現にパースする。
    ///
    /// 外側 `[[]]` を本文から取り出す処理（markdown 走査）は M1 の責務。
    /// ここは「中身の文字列 → 構造体」だけに集中する。
    ///
    /// 区切り文字の取り決め:
    /// - `@` … コード参照の合図（ファイルとシンボルの境目）
    /// - `::` … シンボルのスコープ区切り
    /// - `#` … ドキュメントの見出し区切り
    ///
    /// 判定は単純で、まず `@` があればコード参照。無ければドキュメント参照。
    pub fn parse(inner: &str) -> Reference {
        let inner = inner.trim();

        // --- コード参照: `@` を含む ---
        // split_once は「最初の区切りで 1 回だけ分割」する。
        // 例: "state.c@g_idx" -> ("state.c", "g_idx")
        if let Some((path, sym)) = inner.split_once('@') {
            // シンボル側を "::" で割って各段の名前にする。
            // 前後の空白を除き、空要素は捨てる（"A:: B" のような揺れに耐える）。
            let segments = sym
                .split("::")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return Reference::Code(SymbolDescriptor {
                file: RelPath(normalize_path(path.trim())),
                path: SymbolPath(segments),
                kind: None,
                disambiguator: None,
            });
        }

        // --- ドキュメント参照: 見出し `#` を任意に伴う ---
        let (target_str, heading) = match inner.split_once('#') {
            Some((t, h)) => (t.trim(), Some(h.trim().to_string())),
            None => (inner, None),
        };
        // パスっぽい（`/` を含む or `.md` で終わる）なら Path、
        // そうでなければ短縮名 Name とみなす。
        let target = if target_str.contains('/') || target_str.ends_with(".md") {
            DocTarget::Path(RelPath(normalize_path(target_str)))
        } else {
            DocTarget::Name(target_str.to_string())
        };
        Reference::Doc { target, heading }
    }
}

/// パス区切りを `/` に正規化する小さなヘルパ。
fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}

// ---------------------------------------------------------------------------
// 位置情報（LSP 境界で使う）
// ---------------------------------------------------------------------------

/// ソース上の 1 点。行・桁ともに 0 始まり。
///
/// LSP は桁を UTF-16 単位で数えるという独特な規約があるが、その変換は
/// LSP アダプタ層（`lsp` クレート）の責務にし、コアはこの素朴な表現で扱う。
/// こうしてコアを「特定プロトコルに依存しない」状態に保つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// ソース上の範囲（開始位置〜終了位置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

// ---------------------------------------------------------------------------
// 索引後の表現（解決済み）とグラフ
// ---------------------------------------------------------------------------

/// intern（インターン）済みの軽量シンボルハンドル。
///
/// 「intern」とは、繰り返し現れる値（ここでは [`SymbolDescriptor`]）を
/// 1 か所に 1 個だけ持ち、他は全部その「整理番号」で参照する手法。
/// グラフの辺は嵩張る記述子そのものではなく、この 4 バイトの ID で持つ。
/// これが省メモリ（このプロジェクトの第一目標）の要になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub u32);

/// 索引時に tree-sitter から得る「現在の」シンボル 1 件。
///
/// `descriptor`（不変の同一性）に対し、`range`（可変の現在位置）を
/// 結びつける。**再パースのたびに `range` が更新される**ことで、
/// リンクが自動的に最新の位置へ追従する＝行ズレで切れない、を実現する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub descriptor: SymbolDescriptor,
    /// 定義全体の範囲（関数なら本体まで含む）。
    pub range: Range,
    /// 名前部分だけの範囲。ジャンプ時にここへカーソルを置くと精度が良い。
    pub name_range: Range,
}

/// 参照を解決した結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    /// 一意に解決できた。`id` のシンボルが現在 `range` にある。
    Resolved { id: SymbolId, range: Range },
    /// 候補が複数（C++ のオーバーロード等）。診断で候補を提示し、
    /// ユーザに `disambiguator` で絞ってもらう。
    Ambiguous(Vec<SymbolId>),
    /// 見つからない（リネーム/削除された）。波線＋クイックフィックスで
    /// 直し方を提案する。**黙って間違った場所へ飛ばさない**のが大事。
    Dangling,
}

/// 知識グラフのノード（点）の識別子。
///
/// ノードには 4 種類ある。Markdown ノート、ノート内のセクション、
/// コードシンボル、用語集の用語。グラフビューではこれらを色やアイコンで
/// 区別して描く。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeId {
    /// Markdown ドキュメント。
    Doc(RelPath),
    /// ドキュメント内のセクション（パス + 見出しスラッグ）。
    Section(RelPath, String),
    /// コードシンボル（[`SymbolId`] 経由で記述子を参照）。
    Symbol(SymbolId),
    /// 用語集の用語（スラッグ）。
    Term(String),
}

/// 知識グラフの辺（エッジ）。ノード間のつながりを表す。
///
/// 設計上の重要な決定: **`References` はシンボル間の粒度で 1 本だけ持つ**。
/// ある関数 `tick` がグローバル変数 `g_idx` を 10 回読み書きしていても、
/// 辺は `tick → g_idx` の 1 本。個々の使用箇所（10 個の座標）は保存せず、
/// 「`g_idx` の使用箇所を見せて」と要求された時に live で数え直す。
/// これで、グローバル変数が散らばった汚いコードでも索引が膨れない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    /// ドキュメント → ドキュメント/セクション のリンク（`[[other.md]]`）。
    Links { from: NodeId, to: NodeId },
    /// ドキュメント → コード、またはコード → ドキュメント の参照。
    References { from: NodeId, to: NodeId },
    /// 用語の出現（用語集の自動リンク由来）。
    Mentions { from: NodeId, term: String },
    /// 包含関係（ドキュメント → その中のセクション）。
    Contains { parent: NodeId, child: NodeId },
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------
// `cargo test` で実行される。パーサが意図通り動くかをここで保証する。
// テストは仕様の実例にもなるので、読むと型の使い方が分かる。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_reference_with_scope() {
        // 「Rust/C++ の入れ子シンボル」が正しく段に割れること。
        let r = Reference::parse("lin/protocol.rs@LinFrame::parse");
        assert_eq!(
            r,
            Reference::Code(SymbolDescriptor {
                file: RelPath("lin/protocol.rs".into()),
                path: SymbolPath(vec!["LinFrame".into(), "parse".into()]),
                kind: None,
                disambiguator: None,
            })
        );
    }

    #[test]
    fn parses_flat_c_symbol() {
        // C はスコープが無いので path は要素 1 個になる。
        match Reference::parse("state.c@g_sched_idx") {
            Reference::Code(d) => {
                assert_eq!(d.file, RelPath("state.c".into()));
                assert_eq!(d.path, SymbolPath(vec!["g_sched_idx".into()]));
            }
            other => panic!("expected code reference, got {other:?}"),
        }
    }

    #[test]
    fn parses_doc_with_heading() {
        // 見出し付きドキュメント参照（日本語見出しも通ること）。
        let r = Reference::parse("docs/spec.md#プロトコル");
        assert_eq!(
            r,
            Reference::Doc {
                target: DocTarget::Path(RelPath("docs/spec.md".into())),
                heading: Some("プロトコル".into()),
            }
        );
    }

    #[test]
    fn parses_short_name() {
        // `/` も `.md` も無ければ短縮名として扱う。
        let r = Reference::parse("用語集ノート");
        assert_eq!(
            r,
            Reference::Doc {
                target: DocTarget::Name("用語集ノート".into()),
                heading: None,
            }
        );
    }

    #[test]
    fn normalizes_backslashes() {
        // Windows 風の `\` 区切りが `/` に正規化されること。
        match Reference::parse("src\\app\\main.rs@main") {
            Reference::Code(d) => assert_eq!(d.file, RelPath("src/app/main.rs".into())),
            other => panic!("expected code reference, got {other:?}"),
        }
    }

    /// 観点: normalize_path は `\` を `/` に変換し、それ以外は変えない。
    #[test]
    fn normalize_path_converts_separators() {
        assert_eq!(normalize_path("a\\b\\c"), "a/b/c");
        assert_eq!(normalize_path("a/b/c"), "a/b/c"); // 既に `/` なら無変更
    }
}
