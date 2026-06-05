//! # 索引器（M1/M2）: Markdown を知識グラフに変換し、LSP に知識を提供する
//!
//! このモジュールは「ワークスペースを走査して知識グラフを組み立てる」役割。
//! - `.md` を走査し、見出し（Section）とドキュメント（Doc）をノードにする。
//! - 本文中の `[[...]]` を抽出し、doc→doc リンク（[`Edge::Links`]）を張る。
//! - `.rs` を tree-sitter で解析してシンボルを Symbol ノードにし
//!   （[`crate::code`]）、`[[file.rs@sym]]` を解決して References 辺を張る。
//!
//! 設計上の約束（`CLAUDE.md` の「コアは headless」）に従い、**IO は端に、
//! 解析は純粋関数に** 分ける。[`index_document`] は文字列を受け取る純粋関数で、
//! ファイルシステムに触れない＝テストしやすい。走査 [`index_workspace`] は
//! それを呼ぶ薄いラッパ。
//!
//! M2 で追加した [`parse_links_with_ranges`] は、LSP の DocumentLink/Hover/診断が
//! 必要とする「リンク＋ソース上の位置」を live で返す純粋関数。位置はグラフに
//! 保存しない（「参照は座標ではなくクエリ」の原則）——要求のたびに計算し直す。

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use serde::Serialize;

use crate::code::{extract_symbols, SymbolTable};
use crate::model::{
    self, DocTarget, Edge, NodeId, Reference, RelPath, Resolution, SymbolDescriptor,
};

/// 1 つのドキュメントを解析した結果（位置を持たない中間表現）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDoc {
    /// 見出しテキスト（出現順）。
    pub headings: Vec<String>,
    /// 本文から抽出した `[[...]]` 参照（解決前）。
    pub refs: Vec<Reference>,
}

/// Markdown 文字列を解析し、見出しと `[[...]]` 参照を取り出す **純粋関数**。
///
/// コードブロック・インラインコード内の `[[...]]` はリンク化しない
/// （コード例の中の文字列を誤ってリンク扱いしないため）。判定には
/// pulldown-cmark が返すコード範囲（バイトオフセット）を使う。
pub fn index_document(content: &str) -> ParsedDoc {
    use pulldown_cmark::{Event, Parser, Tag};

    // YAML フロントマター（先頭の `---` 〜 `---`）は本文ではないので先に除く。
    // pulldown-cmark はフロントマターを知らず、`---` を見出し下線などと
    // 誤解釈してしまうため。除いた本文に対して解析する。
    let body = strip_frontmatter(content);

    let mut headings = Vec::new();
    let mut code_ranges: Vec<Range<usize>> = Vec::new();
    let mut in_heading = false;
    let mut heading_buf = String::new();
    let mut codeblock_start: Option<usize> = None;

    // into_offset_iter は (イベント, ソース上のバイト範囲) を返す。
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading(..)) => {
                in_heading = true;
                heading_buf.clear();
            }
            Event::End(Tag::Heading(..)) => {
                in_heading = false;
                let h = heading_buf.trim().to_string();
                if !h.is_empty() {
                    headings.push(h);
                }
            }
            // 見出し内のテキストだけを拾って見出し名を組み立てる。
            Event::Text(t) if in_heading => heading_buf.push_str(&t),
            // フェンス付きコードブロックは Start〜End の範囲を丸ごと除外対象に。
            Event::Start(Tag::CodeBlock(_)) => codeblock_start = Some(range.start),
            Event::End(Tag::CodeBlock(_)) => {
                if let Some(s) = codeblock_start.take() {
                    code_ranges.push(s..range.end);
                }
            }
            // インラインコード `...` も除外対象。
            Event::Code(_) => code_ranges.push(range),
            _ => {}
        }
    }

    let refs = extract_wikilinks(body, &code_ranges);
    ParsedDoc { headings, refs }
}

/// 先頭の YAML フロントマター（`---` で始まり次の `---` 行で閉じるブロック）を
/// 取り除き、本文スライスを返す。フロントマターが無ければそのまま返す。
fn strip_frontmatter(content: &str) -> &str {
    let is_delim = |line: &str| line.trim_end_matches(['\r', '\n']) == "---";

    let mut lines = content.split_inclusive('\n');
    match lines.next() {
        Some(first) if is_delim(first) => {
            let mut offset = first.len();
            for line in lines {
                offset += line.len();
                if is_delim(line) {
                    return &content[offset..]; // 閉じ `---` の次から本文
                }
            }
            // 閉じが見つからなければフロントマターと見なさない（安全側）。
            content
        }
        _ => content,
    }
}

/// 本文から `[[ ... ]]` を走査し、コード範囲内のものを除いて参照に変換する。
fn extract_wikilinks(content: &str, code_ranges: &[Range<usize>]) -> Vec<Reference> {
    let bytes = content.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        // `[` と `]` は ASCII なのでバイト走査で安全（中身の日本語は UTF-8 境界が崩れない）。
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            if let Some(off) = find_subslice(&bytes[inner_start..], b"]]") {
                let inner_end = inner_start + off;
                if !in_any_range(i, code_ranges) {
                    if let Ok(inner) = std::str::from_utf8(&bytes[inner_start..inner_end]) {
                        let inner = inner.trim();
                        if !inner.is_empty() && !inner.contains('\n') {
                            refs.push(Reference::parse(inner));
                        }
                    }
                }
                i = inner_end + 2; // 閉じ `]]` の先へ進む
                continue;
            }
        }
        i += 1;
    }
    refs
}

/// `haystack` 内で最初に `needle` が現れるバイト位置を返す（素朴な探索）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 位置 `pos` がいずれかの範囲に含まれるか。
fn in_any_range(pos: usize, ranges: &[Range<usize>]) -> bool {
    ranges.iter().any(|r| r.contains(&pos))
}

// ---------------------------------------------------------------------------
// ワークスペース全体の索引
// ---------------------------------------------------------------------------

/// 表示用のノード情報（id と人間向けラベル）。
struct NodeInfo {
    id: NodeId,
    label: String,
}

/// 索引で組み立てた知識グラフ。
///
/// M1 ユニット1 ではノード集合と辺の列を保持するだけ（隣接リストや
/// petgraph は、逆参照やグラフビューの探索が要る M3 で導入する）。
pub struct KnowledgeGraph {
    nodes: Vec<NodeInfo>,
    edges: Vec<Edge>,
    seen: HashSet<NodeId>,
    /// 解決できたコードシンボル参照の数。
    pub code_refs_resolved: usize,
    /// 解決できなかった（dangling/ambiguous）コードシンボル参照の数。
    pub code_refs_unresolved: usize,
}

impl Default for KnowledgeGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeGraph {
    pub fn new() -> Self {
        KnowledgeGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            seen: HashSet::new(),
            code_refs_resolved: 0,
            code_refs_unresolved: 0,
        }
    }

    /// ノードを追加（同じ id は一度だけ）。
    fn add_node(&mut self, id: NodeId, label: String) {
        if self.seen.insert(id.clone()) {
            self.nodes.push(NodeInfo { id, label });
        }
    }

    fn add_edge(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 全ノードの ID を走査する（テストやグラフビューの描画に使う）。
    pub fn node_ids(&self) -> impl Iterator<Item = &NodeId> {
        self.nodes.iter().map(|n| &n.id)
    }

    /// 全エッジへの参照（テストやグラフビューの描画に使う）。
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// 機械可読な JSON 文字列に変換する（CLI / エージェント向けの出力契約）。
    ///
    /// 内部の `NodeId` enum をそのまま出すと扱いにくいので、フラットな
    /// 文字列 id（例: `doc:path.md`）に整えて出力する。
    ///
    /// `include_sections` が `false` のとき [`NodeId::Section`] ノードと
    /// [`Edge::Contains`] 辺を除外して出力する。doc 1 本あたり見出し数分だけ
    /// 発生するこれらを省くと、Webview への転送コストと JS のメモリ使用量が
    /// 大きく下がる。CLI は全量が必要なので `true` を渡す。
    pub fn to_json_string(&self, include_sections: bool) -> String {
        let out = GraphJson {
            nodes: self
                .nodes
                .iter()
                .filter(|n| include_sections || !matches!(n.id, NodeId::Section(..)))
                .map(|n| NodeJson {
                    id: node_id_string(&n.id),
                    kind: node_kind(&n.id),
                    label: n.label.clone(),
                })
                .collect(),
            edges: self
                .edges
                .iter()
                .filter(|e| include_sections || !matches!(e, Edge::Contains { .. }))
                .map(edge_json)
                .collect(),
        };
        serde_json::to_string(&out).unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string())
    }
}

// ---------------------------------------------------------------------------
// ワークスペース索引の結果をまとめる型
// ---------------------------------------------------------------------------

/// [`index_workspace`] が返す索引結果。グラフ・シンボル表・ドキュメントパス集合を束ねる。
///
/// LSP サーバは起動後にこれを 1 つ保持し、DocumentLink/Hover/補完/診断の
/// 基盤として使う。グラフには位置を保存しない（「参照はクエリ」の原則）ため、
/// 各リクエスト時に [`parse_links_with_ranges`] で live 計算する。
pub struct WorkspaceIndex {
    /// ノードと辺からなる知識グラフ。グラフビュー・逆参照 CodeLens に使う。
    pub graph: KnowledgeGraph,
    /// コードシンボルの解決表。`[[file.rs@Sym]]` を解決するときに問い合わせる。
    pub symbols: SymbolTable,
    /// ワークスペース内の全 `.md` パス集合（相対パス・`/` 区切り）。
    /// DocumentLink でリンク先が実在するか確認するための高速ルックアップ用。
    pub doc_paths: HashSet<String>,
    /// ファイル名ステム（拡張子・ディレクトリなし）→ パス。
    /// 一意に定まる場合は `Some(path)`、同名が複数ある場合は `None`（短縮名解決用）。
    /// 例: `"spec"` → `Some(RelPath("docs/spec.md"))` なら `[[spec]]` が解決できる。
    pub doc_by_stem: HashMap<String, Option<RelPath>>,
    /// `glossary/` 配下のノートから構築した用語集（D8 の仮想リンク用）。
    /// 用語の出現位置は保存せず、[`find_term_mentions`] で要求時に算出する。
    pub glossary: Glossary,
    /// 重複定義されている用語名の集合（M5-4 Linter 診断用）。
    ///
    /// `glossary/` 配下で同一ファイル名ステムを持つノートが 2 つ以上ある場合に登録される。
    /// 例: `glossary/LIN.md` と `glossary/sub/LIN.md` が共存すると `"LIN"` が入る。
    /// LSP アダプタはこれを使い、該当する用語ノートに Warning 診断を出す。
    pub duplicate_terms: HashSet<String>,
}

/// [`WorkspaceIndex::resolve_reference`] が返す解決結果。
///
/// この型は `dbrain-core` 側に置く——「賢さはコアに」の原則より、
/// LSP / MCP / CLI が同じロジックを重複実装しないよう共有する。
/// エディタ側アダプタはこれをパターンマッチして各プロトコルの型に変換するだけでよい。
#[derive(Debug)]
pub enum ReferenceResolution {
    /// ドキュメントリンクが一意に解決できた。
    DocResolved(RelPath),
    /// 同名ステムが複数あり、どのドキュメントか一意に定まらない。
    /// フルパス指定に直すよう案内する。
    DocAmbiguous,
    /// 対応するドキュメントが存在しない。
    DocDangling,
    /// コードシンボルが一意に解決できた。`id` で [`SymbolTable::get`] を引ける。
    CodeResolved { id: crate::model::SymbolId },
    /// 同名・同パスのシンボルが複数（C++ オーバーロード等）。
    CodeAmbiguous(Vec<crate::model::SymbolId>),
    /// 対応するシンボルが見つからない（リネームや削除後の dangling 参照）。
    CodeDangling,
}

impl WorkspaceIndex {
    /// `[[...]]` 参照を索引に照らして解決する。
    ///
    /// エディタ機能（DocumentLink・GotoDefinition・Hover・診断）はこれを呼び、
    /// 返り値をパターンマッチして各プロトコルの型に変換する。
    /// 解決ロジックが 1 か所に集約されるため、LSP・MCP・CLI で実装を重複させない。
    pub fn resolve_reference(&self, reference: &Reference) -> ReferenceResolution {
        match reference {
            Reference::Doc { target, .. } => match target {
                DocTarget::Path(p) => {
                    if self.doc_paths.contains(&p.0) {
                        ReferenceResolution::DocResolved(p.clone())
                    } else {
                        ReferenceResolution::DocDangling
                    }
                }
                DocTarget::Name(name) => match self.doc_by_stem.get(name.as_str()) {
                    Some(Some(path)) => ReferenceResolution::DocResolved(path.clone()),
                    Some(None) => ReferenceResolution::DocAmbiguous,
                    None => ReferenceResolution::DocDangling,
                },
            },
            Reference::Code(desc) => match self.symbols.resolve(desc) {
                Resolution::Resolved { id, .. } => ReferenceResolution::CodeResolved { id },
                Resolution::Ambiguous(ids) => ReferenceResolution::CodeAmbiguous(ids),
                Resolution::Dangling => ReferenceResolution::CodeDangling,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 位置付きリンク抽出（LSP の DocumentLink / Hover / 診断向け）
// ---------------------------------------------------------------------------

/// ドキュメント内の `[[...]]` を **ソース上の位置付き** で抽出する。
///
/// グラフ（[`KnowledgeGraph`]）には位置を保存しない——「参照は座標ではなくクエリ」の
/// 原則に従い、位置は要求のたびに live で計算し直す（`CLAUDE.md` 不変条件①）。
/// この関数はその live 計算の実体であり、LSP ハンドラから毎回呼ばれることを前提にする。
///
/// - コードブロック・インラインコード内の `[[...]]` は除外する（[`index_document`] と同じ）。
/// - フロントマターを除去してから解析するが、返す位置はフロントマターを含む
///   **ドキュメント全体** の行・文字位置（LSP が扱うのはファイル全体のため）。
/// - 文字位置は UTF-8 コードポイント数（Rust の `char` 数）。LSP が要求する
///   UTF-16 単位への変換は `lsp` クレートの責務。
pub fn parse_links_with_ranges(content: &str) -> Vec<(Reference, model::Range)> {
    let body = strip_frontmatter(content);
    // body は content の末尾スライス。ポインタ差でフロントマターのバイト数を得る。
    // Safety: body は content のサブスライスが保証されている（strip_frontmatter の実装より）。
    let fm_bytes = body.as_ptr() as usize - content.as_ptr() as usize;

    let code_ranges = collect_code_ranges(body);

    // body をバイト走査して [[...]] を見つけ、位置を付けて返す。
    let bytes = body.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            if let Some(off) = find_subslice(&bytes[inner_start..], b"]]") {
                let inner_end = inner_start + off;
                if !in_any_range(i, &code_ranges) {
                    if let Ok(inner) = std::str::from_utf8(&bytes[inner_start..inner_end]) {
                        let inner = inner.trim();
                        if !inner.is_empty() && !inner.contains('\n') {
                            // body 上のオフセット → content 上の絶対オフセット に変換。
                            let abs_start = fm_bytes + i;
                            let abs_end = fm_bytes + inner_end + 2; // `]]` の直後
                            let start = byte_offset_to_position(content, abs_start);
                            let end = byte_offset_to_position(content, abs_end);
                            result.push((Reference::parse(inner), model::Range { start, end }));
                        }
                    }
                }
                i = inner_end + 2;
                continue;
            }
        }
        i += 1;
    }
    result
}

/// バイトオフセット → 行・文字位置（0 始まり）に変換するヘルパ。
///
/// 「文字」は UTF-8 コードポイント数（Rust の `char` 数）。
/// LSP の UTF-16 変換は LSP アダプタ層で行う。
fn byte_offset_to_position(text: &str, byte_offset: usize) -> model::Position {
    let prefix = &text[..byte_offset.min(text.len())];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() as u32;
    let last_nl = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let character = prefix[last_nl..].chars().count() as u32;
    model::Position { line, character }
}

/// 本文中のコードブロック・インラインコードのバイト範囲を集める。
///
/// `[[...]]` リンクや用語の出現を、コード例の中で誤検出しないための除外範囲。
/// [`parse_links_with_ranges`] と [`find_term_mentions`] が共有する。
fn collect_code_ranges(body: &str) -> Vec<Range<usize>> {
    use pulldown_cmark::{Event, Parser, Tag};

    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut codeblock_start: Option<usize> = None;
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => codeblock_start = Some(range.start),
            Event::End(Tag::CodeBlock(_)) => {
                if let Some(s) = codeblock_start.take() {
                    ranges.push(s..range.end);
                }
            }
            Event::Code(_) => ranges.push(range),
            _ => {}
        }
    }
    ranges
}

/// 本文中の `[[...]]` リンク全体のバイト範囲を集める。
///
/// 用語の自動リンク（[`find_term_mentions`]）が、既に明示リンクが張られた箇所に
/// 二重でリンクを作らないための除外範囲。
fn collect_wikilink_ranges(body: &str) -> Vec<Range<usize>> {
    let bytes = body.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(off) = find_subslice(&bytes[i + 2..], b"]]") {
                let end = i + 2 + off + 2; // 閉じ `]]` の直後
                ranges.push(i..end);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    ranges
}

// ---------------------------------------------------------------------------
// 用語集（M5: D8 非破壊な仮想リンク）
// ---------------------------------------------------------------------------

/// 用語集ノートを置くディレクトリ名。この配下の `.md` が用語エントリになる。
/// 将来は設定可能にする余地があるが、まずは固定。
const GLOSSARY_DIR: &str = "glossary";

/// 用語集の 1 エントリ。`glossary/` 配下の 1 つの `.md` ノートに対応する。
///
/// 用語名はファイル名ステム（`glossary/LIN.md` → `"LIN"`）、`definition` は
/// Hover 表示用の定義抜粋（本文の先頭段落）。本文自体は書き換えない（D8）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryTerm {
    /// 正規の用語名（ファイル名ステム）。
    pub name: String,
    /// 用語ノートのパス（リンクの飛び先）。
    pub path: RelPath,
    /// Hover 表示用の定義抜粋。
    pub definition: String,
}

/// 用語集。出現検出のための「表層形 → 用語」マッチング表を内部に持つ。
///
/// メモリは用語数に比例するだけ（出現位置は保存せず、要求時に
/// [`find_term_mentions`] でライブ計算する）。これは References 辺と同じ思想
/// （`CLAUDE.md` 不変条件①③）。
#[derive(Debug, Default)]
pub struct Glossary {
    terms: Vec<GlossaryTerm>,
    /// (表層形, terms 内インデックス) を長さ降順に並べたもの。最長一致のため。
    patterns: Vec<(String, usize)>,
}

impl Glossary {
    /// 用語リストからマッチング表を構築する。
    pub fn new(terms: Vec<GlossaryTerm>) -> Self {
        // 表層形は今は用語名のみ（別名 aliases は後続サブタスクで追加予定）。
        let mut patterns: Vec<(String, usize)> = terms
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        // 長い表層形を先に試すことで最長一致を実現する
        // （"状態遷移" を "状態" より優先）。
        patterns.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        Glossary { terms, patterns }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// インデックスから用語エントリを取得する。
    pub fn get(&self, index: usize) -> Option<&GlossaryTerm> {
        self.terms.get(index)
    }

    /// 全用語エントリを走査する。
    pub fn terms(&self) -> &[GlossaryTerm] {
        &self.terms
    }
}

/// 用語の出現 1 件（位置付き）。位置はグラフに保存せず、要求時に算出する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermMention {
    /// [`Glossary`] 内の用語インデックス。
    pub term_index: usize,
    /// 出現箇所のソース上の範囲。
    pub range: model::Range,
}

/// 本文から用語の出現を **全件・位置付き** で検出する純粋関数（D8 の仮想リンクの実体）。
///
/// - コードブロック・インラインコード・既存の `[[...]]` リンク内は除外する。
/// - ASCII 用語は単語境界を要求する（"LIN" が "LINUX" にマッチしない）。
///   CJK を含む用語は単語境界が無いので部分一致を許す。
/// - 最長一致優先（[`Glossary::new`] が表層形を長さ降順に並べてある）。
/// - `current_path` が用語自身の定義ノートの場合、その用語は自己リンクしない。
/// - 同じ用語が複数回出てもすべて返す（最初の 1 回に限定しない）。
pub fn find_term_mentions(
    content: &str,
    glossary: &Glossary,
    current_path: &RelPath,
) -> Vec<TermMention> {
    if glossary.is_empty() {
        return Vec::new();
    }

    let body = strip_frontmatter(content);
    let fm_bytes = body.as_ptr() as usize - content.as_ptr() as usize;
    let code_ranges = collect_code_ranges(body);
    let link_ranges = collect_wikilink_ranges(body);

    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // コード範囲・既存リンク範囲の中はスキップ。
        if in_any_range(i, &code_ranges) || in_any_range(i, &link_ranges) {
            i += utf8_char_width(bytes[i]);
            continue;
        }

        // この位置で最長一致する用語を探す（patterns は長さ降順）。
        let mut matched_len = 0;
        for (pat, term_idx) in &glossary.patterns {
            let pb = pat.as_bytes();
            if i + pb.len() > bytes.len() || &bytes[i..i + pb.len()] != pb {
                continue;
            }
            // ASCII 用語は単語境界を要求する。
            if pat.is_ascii() && !ascii_word_boundary_ok(bytes, i, pb.len()) {
                continue;
            }
            matched_len = pb.len();
            // 用語自身の定義ノートでは自己リンクしない（マッチはするが記録しない）。
            if glossary.terms[*term_idx].path != *current_path {
                let start = byte_offset_to_position(content, fm_bytes + i);
                let end = byte_offset_to_position(content, fm_bytes + i + pb.len());
                out.push(TermMention {
                    term_index: *term_idx,
                    range: model::Range { start, end },
                });
            }
            break;
        }

        // マッチしたらその分進める。しなければ 1 文字進める（UTF-8 境界を保つ）。
        i += if matched_len > 0 {
            matched_len
        } else {
            utf8_char_width(bytes[i])
        };
    }
    out
}

/// UTF-8 の先頭バイトから、その文字のバイト幅（1〜4）を返す。
fn utf8_char_width(first_byte: u8) -> usize {
    match first_byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// ASCII 用語の単語境界判定。マッチ範囲の前後が「単語を構成しない」なら true。
///
/// 単語構成文字 = ASCII 英数字 or `_`。例: "LIN" は "LINUX" の中ではマッチさせない。
fn ascii_word_boundary_ok(bytes: &[u8], start: usize, len: usize) -> bool {
    let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
    let end = start + len;
    let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
    before_ok && after_ok
}

/// 単語を構成するバイトか（ASCII 英数字または `_`）。
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---------------------------------------------------------------------------
// ワークスペース全体の索引
// ---------------------------------------------------------------------------

/// ワークスペースを走査して知識グラフを組み立てる。
pub fn index_workspace(root: &Path) -> WorkspaceIndex {
    let mut graph = KnowledgeGraph::new();

    // --- pass 0: ソース（Rust/C）からシンボル表を作り、Symbol ノードを追加 ---
    let mut symbols = SymbolTable::default();
    for ext in ["rs", "c", "h"] {
        for path in walk_by_ext(root, ext) {
            let rel = to_rel(root, &path);
            let src = std::fs::read_to_string(&path).unwrap_or_default();
            for entry in extract_symbols(&rel, &src) {
                symbols.intern(entry);
            }
        }
    }
    for (id, entry) in symbols.iter() {
        graph.add_node(NodeId::Symbol(id), symbol_label(&entry.descriptor));
    }

    // --- pass 1: 全 .md を読み、Doc/Section ノードと Contains 辺を作る ---
    // content を保持するのは pass 3 で find_term_mentions に再利用するため。
    // 同時に glossary/ 配下のノートを用語集エントリとして収集する（D8）。
    let mut docs: Vec<(RelPath, ParsedDoc, String)> = Vec::new();
    let mut glossary_terms: Vec<GlossaryTerm> = Vec::new();
    for path in walk_by_ext(root, "md") {
        let rel = to_rel(root, &path);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        if is_glossary_note(&rel.0) {
            if let Some(name) = file_stem(&rel.0) {
                glossary_terms.push(GlossaryTerm {
                    name,
                    path: rel.clone(),
                    definition: glossary_excerpt(&content),
                });
            }
        }
        docs.push((rel, index_document(&content), content));
    }

    // 重複定義の検出: 同一用語名（ファイル名ステム）が 2 つ以上ある用語を集める。
    // Glossary::new に渡す前に集計することで、所有権の移動と分離できる。
    let mut stem_count: HashMap<String, usize> = HashMap::new();
    for t in &glossary_terms {
        *stem_count.entry(t.name.clone()).or_insert(0) += 1;
    }
    let duplicate_terms: HashSet<String> = stem_count
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();

    let glossary = Glossary::new(glossary_terms);

    let mut doc_paths: HashSet<String> = HashSet::new();
    // 短縮名（ファイル名ステム）→ パス。一意なものだけ解決に使う。
    let mut stem_to_path: HashMap<String, Option<RelPath>> = HashMap::new();

    for (rel, parsed, _content) in &docs {
        doc_paths.insert(rel.0.clone());
        if let Some(stem) = file_stem(&rel.0) {
            stem_to_path
                .entry(stem)
                .and_modify(|e| *e = None) // 既出＝重複なので解決不能に倒す
                .or_insert_with(|| Some(rel.clone()));
        }

        let doc = NodeId::Doc(rel.clone());
        graph.add_node(doc.clone(), rel.0.clone());
        for h in &parsed.headings {
            let sec = NodeId::Section(rel.clone(), h.clone());
            graph.add_node(sec.clone(), h.clone());
            graph.add_edge(Edge::Contains {
                parent: doc.clone(),
                child: sec,
            });
        }
    }

    // --- pass 2: 参照を解決して辺を張る ---
    for (rel, parsed, _content) in &docs {
        let from = NodeId::Doc(rel.clone());
        for r in &parsed.refs {
            match r {
                Reference::Doc { target, .. } => {
                    if let Some(to_path) = resolve_doc_target(target, &doc_paths, &stem_to_path) {
                        graph.add_edge(Edge::Links {
                            from: from.clone(),
                            to: NodeId::Doc(to_path),
                        });
                    }
                    // 解決できないリンクは unit1 では辺を張らない（診断は M2）。
                }
                Reference::Code(desc) => match symbols.resolve(desc) {
                    Resolution::Resolved { id, .. } => {
                        graph.add_edge(Edge::References {
                            from: from.clone(),
                            to: NodeId::Symbol(id),
                        });
                        graph.code_refs_resolved += 1;
                    }
                    // Dangling / Ambiguous は数えるだけ。波線＝診断は M2 で LSP 側に出す。
                    Resolution::Dangling | Resolution::Ambiguous(_) => {
                        graph.code_refs_unresolved += 1;
                    }
                },
            }
        }
    }

    // --- pass 3: 用語の出現をグラフに反映（Term ノード + Mentions 辺）---
    // 出現があった用語だけ Term ノードにする（省メモリ）。
    // 同一ドキュメントに同じ用語が複数回出現しても辺は 1 本に集約する。
    for (rel, _parsed, content) in &docs {
        let mentions = find_term_mentions(content, &glossary, rel);
        // 同一ドキュメント内で既に辺を張った term_index を記録する。
        let mut seen_in_doc: HashSet<usize> = HashSet::new();
        for m in mentions {
            if seen_in_doc.insert(m.term_index) {
                let term = &glossary.terms()[m.term_index];
                // 初めて出現した用語は Term ノードも追加（add_node は重複を排除する）。
                graph.add_node(NodeId::Term(term.name.clone()), term.name.clone());
                graph.add_edge(Edge::Mentions {
                    from: NodeId::Doc(rel.clone()),
                    term: term.name.clone(),
                });
            }
        }
    }

    WorkspaceIndex {
        graph,
        symbols,
        doc_paths,
        doc_by_stem: stem_to_path,
        glossary,
        duplicate_terms,
    }
}

/// 相対パスが用語集ノートか（先頭ディレクトリが `glossary`）を判定する。
fn is_glossary_note(rel: &str) -> bool {
    rel.split('/').next() == Some(GLOSSARY_DIR)
}

/// 用語集ノートの本文から Hover 表示用の定義抜粋を取り出す。
///
/// フロントマターと見出し記号を除いた最初の非空行を返す（簡潔さ優先）。
fn glossary_excerpt(content: &str) -> String {
    strip_frontmatter(content)
        .lines()
        .map(|l| l.trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Symbol ノードの表示ラベル（記述子そのものを読める形に）。例: `core/src/model.rs@SymbolDescriptor`。
fn symbol_label(desc: &SymbolDescriptor) -> String {
    format!("{}@{}", desc.file.0, desc.path.0.join("::"))
}

/// `[[名前]]` や `[[path.md]]` を実在ドキュメントのパスに解決する。
fn resolve_doc_target(
    target: &DocTarget,
    doc_paths: &HashSet<String>,
    stem_to_path: &HashMap<String, Option<RelPath>>,
) -> Option<RelPath> {
    match target {
        // パス指定: 実在する場合のみ解決。
        DocTarget::Path(p) => {
            if doc_paths.contains(&p.0) {
                Some(p.clone())
            } else {
                None
            }
        }
        // 短縮名: ステムが一意に定まる場合のみ解決。
        DocTarget::Name(name) => match stem_to_path.get(name) {
            Some(Some(path)) => Some(path.clone()),
            _ => None,
        },
    }
}

/// ルート以下の指定拡張子のファイルを列挙する。`target` / `node_modules` /
/// `.git` は走査対象から外す。
fn walk_by_ext(root: &Path, ext: &str) -> Vec<std::path::PathBuf> {
    use walkdir::WalkDir;

    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(name == "target" || name == "node_modules" || name == ".git")
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
        .collect()
}

/// 絶対パスをワークスペース相対の正規化パス（`/` 区切り）にする。
fn to_rel(root: &Path, path: &Path) -> RelPath {
    let rel = path.strip_prefix(root).unwrap_or(path);
    RelPath(rel.to_string_lossy().replace('\\', "/"))
}

/// `docs/spec.md` → `spec`。拡張子とディレクトリを除いたステム。
fn file_stem(rel: &str) -> Option<String> {
    Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// JSON 出力用 DTO（内部型と公開スキーマを分離する）
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct GraphJson {
    nodes: Vec<NodeJson>,
    edges: Vec<EdgeJson>,
}

#[derive(Serialize)]
struct NodeJson {
    id: String,
    kind: &'static str,
    label: String,
}

#[derive(Serialize)]
struct EdgeJson {
    kind: &'static str,
    from: String,
    to: String,
}

fn node_id_string(id: &NodeId) -> String {
    match id {
        NodeId::Doc(p) => format!("doc:{}", p.0),
        NodeId::Section(p, slug) => format!("section:{}#{}", p.0, slug),
        NodeId::Symbol(s) => format!("symbol:{}", s.0),
        NodeId::Term(t) => format!("term:{t}"),
    }
}

fn node_kind(id: &NodeId) -> &'static str {
    match id {
        NodeId::Doc(_) => "doc",
        NodeId::Section(..) => "section",
        NodeId::Symbol(_) => "symbol",
        NodeId::Term(_) => "term",
    }
}

fn edge_json(edge: &Edge) -> EdgeJson {
    match edge {
        Edge::Links { from, to } => EdgeJson {
            kind: "links",
            from: node_id_string(from),
            to: node_id_string(to),
        },
        Edge::References { from, to } => EdgeJson {
            kind: "references",
            from: node_id_string(from),
            to: node_id_string(to),
        },
        Edge::Mentions { from, term } => EdgeJson {
            kind: "mentions",
            from: node_id_string(from),
            to: format!("term:{term}"),
        },
        Edge::Contains { parent, child } => EdgeJson {
            kind: "contains",
            from: node_id_string(parent),
            to: node_id_string(child),
        },
    }
}

// ===========================================================================
// 単体テスト
// ===========================================================================
//
// 索引器の各関数を個別に検証する。純粋なヘルパ（find_subslice / in_any_range /
// strip_frontmatter / extract_wikilinks / resolve_doc_target / to_rel /
// file_stem / symbol_label / node_id_string / node_kind / edge_json）は直接、
// fs を伴う walk_by_ext / index_workspace は一時ディレクトリで結合的に検証する。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SymbolId, SymbolPath};

    // ----- 低レベルヘルパ: find_subslice / in_any_range -----

    /// 観点: 部分バイト列の最初の出現位置を返す。無ければ None。
    #[test]
    fn find_subslice_locates_or_none() {
        assert_eq!(find_subslice(b"abXYcd", b"XY"), Some(2));
        assert_eq!(find_subslice(b"abc", b"ZZ"), None);
    }

    /// 観点: 位置が範囲群のいずれかに含まれるかを正しく判定する。
    #[test]
    fn in_any_range_membership() {
        let ranges = vec![2..5, 10..12];
        assert!(in_any_range(3, &ranges)); // 2..5 の内側
        assert!(!in_any_range(5, &ranges)); // 上端は排他
        assert!(!in_any_range(8, &ranges)); // どの範囲にもない
        assert!(in_any_range(10, &ranges)); // 下端は含む
    }

    // ----- strip_frontmatter -----

    /// 観点: 先頭 YAML フロントマターを除去し本文を返す。無ければそのまま。
    #[test]
    fn strip_frontmatter_cases() {
        assert_eq!(
            strip_frontmatter("---\nk: v\n---\n本文\n"),
            "本文\n",
            "閉じ --- の後ろを本文として返す"
        );
        assert_eq!(
            strip_frontmatter("# 見出し\n本文"),
            "# 見出し\n本文",
            "フロントマターが無ければ無変更"
        );
        // 閉じが無いものはフロントマターと見なさない（安全側＝無変更）。
        assert_eq!(strip_frontmatter("---\nk: v\n本文"), "---\nk: v\n本文");
    }

    // ----- extract_wikilinks -----

    /// 観点: 本文の `[[...]]` を抽出し、コード範囲内のものは除外する。
    #[test]
    fn extract_wikilinks_respects_code_ranges() {
        let text = "a [[one.md]] b [[two.md]] c";
        // コード範囲なし → 2 件。
        let all = extract_wikilinks(text, &[]);
        assert_eq!(all.len(), 2);
        // 2 個目を覆うコード範囲を渡すと 1 件に減る。
        let idx = text.find("[[two.md]]").unwrap();
        let r = idx..idx + 10;
        let masked = extract_wikilinks(text, std::slice::from_ref(&r));
        assert_eq!(masked.len(), 1);
    }

    // ----- index_document（headings / refs / frontmatter / コード除外） -----

    /// 観点: 見出しと doc リンクを取り出す（日本語見出し・短縮名を含む）。
    #[test]
    fn index_document_headings_and_links() {
        let md = "# 見出しA\n\n本文 [[other.md]] と [[名前]]。\n\n## 見出しB\n";
        let p = index_document(md);
        assert_eq!(
            p.headings,
            vec!["見出しA".to_string(), "見出しB".to_string()]
        );
        assert_eq!(p.refs.len(), 2);
    }

    /// 観点: コードブロック／インラインコード内の `[[...]]` はリンク化しない。
    #[test]
    fn index_document_ignores_links_in_code() {
        let md = "本文 [[real.md]]\n\n```\n[[not-a-link.md]]\n```\n\nインライン `[[nope]]` 終わり";
        let p = index_document(md);
        assert_eq!(p.refs.len(), 1);
        match &p.refs[0] {
            Reference::Doc {
                target: DocTarget::Path(rp),
                ..
            } => assert_eq!(rp.0, "real.md"),
            other => panic!("expected doc link to real.md, got {other:?}"),
        }
    }

    /// 観点: 本文中の `[[file.rs@Sym]]` はコード参照として解析される。
    #[test]
    fn index_document_detects_code_reference() {
        let md = "詳細は [[lib.rs@Foo::bar]] を参照。";
        let p = index_document(md);
        assert_eq!(p.refs.len(), 1);
        assert!(matches!(p.refs[0], Reference::Code(_)));
    }

    /// 観点: フロントマター内の `name:` 等は見出しにせず、本文の見出しだけ拾う。
    #[test]
    fn index_document_strips_frontmatter() {
        let md = "---\nname: x\ntitle: \"[bug]\"\n---\n\n# 本当の見出し\n\n[[a.md]]\n";
        let p = index_document(md);
        assert_eq!(p.headings, vec!["本当の見出し".to_string()]);
        assert_eq!(p.refs.len(), 1);
    }

    // ----- resolve_doc_target -----

    /// 観点: パス指定は実在時のみ解決。短縮名は一意時のみ解決、重複・不在は None。
    #[test]
    fn resolve_doc_target_rules() {
        let mut doc_paths = HashSet::new();
        doc_paths.insert("docs/spec.md".to_string());
        doc_paths.insert("a.md".to_string());

        let mut stems: HashMap<String, Option<RelPath>> = HashMap::new();
        stems.insert("spec".into(), Some(RelPath("docs/spec.md".into()))); // 一意
        stems.insert("dup".into(), None); // 重複扱い

        // パス: 実在
        assert_eq!(
            resolve_doc_target(
                &DocTarget::Path(RelPath("docs/spec.md".into())),
                &doc_paths,
                &stems
            ),
            Some(RelPath("docs/spec.md".into()))
        );
        // パス: 不在 → None
        assert_eq!(
            resolve_doc_target(
                &DocTarget::Path(RelPath("missing.md".into())),
                &doc_paths,
                &stems
            ),
            None
        );
        // 短縮名: 一意 → 解決
        assert_eq!(
            resolve_doc_target(&DocTarget::Name("spec".into()), &doc_paths, &stems),
            Some(RelPath("docs/spec.md".into()))
        );
        // 短縮名: 重複 → None
        assert_eq!(
            resolve_doc_target(&DocTarget::Name("dup".into()), &doc_paths, &stems),
            None
        );
        // 短縮名: 未知 → None
        assert_eq!(
            resolve_doc_target(&DocTarget::Name("unknown".into()), &doc_paths, &stems),
            None
        );
    }

    // ----- to_rel / file_stem / symbol_label -----

    /// 観点: 絶対パスをルート相対・`/` 区切りに正規化する。
    #[test]
    fn to_rel_normalizes() {
        let root = Path::new("/work/proj");
        let p = Path::new("/work/proj/src/lib.rs");
        assert_eq!(to_rel(root, p), RelPath("src/lib.rs".into()));
    }

    /// 観点: パスからファイル名ステム（拡張子・ディレクトリ除去）を得る。
    #[test]
    fn file_stem_extracts() {
        assert_eq!(file_stem("docs/spec.md"), Some("spec".to_string()));
        assert_eq!(file_stem("README.md"), Some("README".to_string()));
    }

    /// 観点: Symbol ノードのラベルは `ファイル@修飾名` の読める形になる。
    #[test]
    fn symbol_label_format() {
        let desc = SymbolDescriptor {
            file: RelPath("core/src/model.rs".into()),
            path: SymbolPath(vec!["Foo".into(), "bar".into()]),
            kind: None,
            disambiguator: None,
        };
        assert_eq!(symbol_label(&desc), "core/src/model.rs@Foo::bar");
    }

    // ----- node_id_string / node_kind / edge_json（出力 DTO 変換） -----

    /// 観点: 各 NodeId 変種が安定したフラット文字列 id になる。
    #[test]
    fn node_id_string_per_variant() {
        assert_eq!(
            node_id_string(&NodeId::Doc(RelPath("a.md".into()))),
            "doc:a.md"
        );
        assert_eq!(
            node_id_string(&NodeId::Section(RelPath("a.md".into()), "見出し".into())),
            "section:a.md#見出し"
        );
        assert_eq!(node_id_string(&NodeId::Symbol(SymbolId(7))), "symbol:7");
        assert_eq!(node_id_string(&NodeId::Term("用語".into())), "term:用語");
    }

    /// 観点: 各 NodeId 変種の種別ラベルが正しい。
    #[test]
    fn node_kind_per_variant() {
        assert_eq!(node_kind(&NodeId::Doc(RelPath("a.md".into()))), "doc");
        assert_eq!(
            node_kind(&NodeId::Section(RelPath("a.md".into()), "h".into())),
            "section"
        );
        assert_eq!(node_kind(&NodeId::Symbol(SymbolId(0))), "symbol");
        assert_eq!(node_kind(&NodeId::Term("t".into())), "term");
    }

    /// 観点: 各 Edge 変種が正しい kind/from/to の DTO に変換される。
    #[test]
    fn edge_json_per_variant() {
        let a = NodeId::Doc(RelPath("a.md".into()));
        let b = NodeId::Doc(RelPath("b.md".into()));
        let links = edge_json(&Edge::Links {
            from: a.clone(),
            to: b.clone(),
        });
        assert_eq!(
            (links.kind, links.from.as_str(), links.to.as_str()),
            ("links", "doc:a.md", "doc:b.md")
        );

        let refs = edge_json(&Edge::References {
            from: a.clone(),
            to: NodeId::Symbol(SymbolId(3)),
        });
        assert_eq!((refs.kind, refs.to.as_str()), ("references", "symbol:3"));

        let men = edge_json(&Edge::Mentions {
            from: a.clone(),
            term: "用語".into(),
        });
        assert_eq!((men.kind, men.to.as_str()), ("mentions", "term:用語"));

        let con = edge_json(&Edge::Contains {
            parent: a,
            child: NodeId::Section(RelPath("a.md".into()), "h".into()),
        });
        assert_eq!((con.kind, con.to.as_str()), ("contains", "section:a.md#h"));
    }

    // ----- KnowledgeGraph: 構築・重複排除・出力 -----

    /// 観点: add_node は同一 id を重複登録しない。count とアクセサが整合する。
    #[test]
    fn graph_dedups_nodes_and_counts() {
        let mut g = KnowledgeGraph::new();
        let doc = NodeId::Doc(RelPath("a.md".into()));
        g.add_node(doc.clone(), "a.md".into());
        g.add_node(doc.clone(), "a.md".into()); // 2 回目は無視される
        g.add_edge(Edge::Links {
            from: doc,
            to: NodeId::Doc(RelPath("b.md".into())),
        });
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.node_ids().count(), 1);
        assert_eq!(g.edges().len(), 1);
    }

    /// 観点: to_json_string が期待スキーマ（id/kind/label, kind/from/to）になる。
    #[test]
    fn graph_json_shape() {
        let mut g = KnowledgeGraph::new();
        let doc = NodeId::Doc(RelPath("a.md".into()));
        g.add_node(doc.clone(), "a.md".into());
        g.add_edge(Edge::Links {
            from: doc,
            to: NodeId::Doc(RelPath("b.md".into())),
        });
        let json = g.to_json_string(true);
        assert!(json.contains("\"id\":\"doc:a.md\""));
        assert!(json.contains("\"kind\":\"links\""));
    }

    // ----- index_workspace（walk_by_ext を含む結合動作） -----

    /// 観点: 一時ワークスペースで、.md の `[[file.rs@Sym]]` が .rs の実シンボルに
    /// 解決して References 辺が張られること（resolved=1 / unresolved=0）。
    #[test]
    fn workspace_resolves_code_reference() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_unit_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("lib.rs"),
            "pub struct Foo;\nimpl Foo { pub fn bar() {} }\n",
        )
        .unwrap();
        fs::write(dir.join("doc.md"), "詳細は [[lib.rs@Foo::bar]] を参照。\n").unwrap();

        let idx = index_workspace(&dir);
        assert_eq!(idx.graph.code_refs_resolved, 1);
        assert_eq!(idx.graph.code_refs_unresolved, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 観点: 不在シンボルへの参照は未解決として数えられ、辺は張られない。
    #[test]
    fn workspace_dangling_code_reference() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_dangle_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("lib.rs"), "pub fn exists() {}\n").unwrap();
        fs::write(dir.join("doc.md"), "[[lib.rs@does_not_exist]]\n").unwrap();

        let idx = index_workspace(&dir);
        assert_eq!(idx.graph.code_refs_resolved, 0);
        assert_eq!(idx.graph.code_refs_unresolved, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    // ----- parse_links_with_ranges / byte_offset_to_position -----

    /// 観点: `[[...]]` の開始・終了位置が行・文字単位で正しく計算されること。
    /// "See [[other.md]] for details." の `[[` は行 2 の文字 4 から始まる。
    #[test]
    fn parse_links_with_ranges_returns_positions() {
        let md = "# Head\n\nSee [[other.md]] for details.\n";
        let links = parse_links_with_ranges(md);
        assert_eq!(links.len(), 1, "リンクが 1 件抽出されること");
        // "See " は 4 文字なので [[other.md]] の開始は char 4。
        assert_eq!(links[0].1.start.line, 2);
        assert_eq!(links[0].1.start.character, 4);
        // "[[other.md]]" は 12 文字なので終了は char 16。
        assert_eq!(links[0].1.end.line, 2);
        assert_eq!(links[0].1.end.character, 16);
    }

    /// 観点: フロントマターの行数が位置に正しく加算されること。
    /// フロントマター 3 行（0-2）＋空行（3）＋"# Head"（4）＋空行（5）＋本文（6）。
    #[test]
    fn parse_links_with_ranges_accounts_for_frontmatter() {
        let md = "---\ntitle: x\n---\n\n# Head\n\nSee [[other.md]].\n";
        let links = parse_links_with_ranges(md);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1.start.line, 6, "フロントマター 3 行分ずれること");
        assert_eq!(links[0].1.start.character, 4);
    }

    /// 観点: コードブロック内の `[[...]]` は位置付き抽出でも除外されること。
    #[test]
    fn parse_links_with_ranges_excludes_code_blocks() {
        let md = "本文 [[real.md]]\n\n```\n[[not-a-link.md]]\n```\n";
        let links = parse_links_with_ranges(md);
        assert_eq!(links.len(), 1);
        match &links[0].0 {
            Reference::Doc {
                target: DocTarget::Path(p),
                ..
            } => {
                assert_eq!(p.0, "real.md")
            }
            other => panic!("expected doc link to real.md, got {other:?}"),
        }
    }

    // ----- 用語集（M5: find_term_mentions / Glossary / is_glossary_note 等） -----

    /// テスト用の用語集を組み立てる小道具。
    fn glossary_of(terms: &[(&str, &str)]) -> Glossary {
        Glossary::new(
            terms
                .iter()
                .map(|(name, path)| GlossaryTerm {
                    name: name.to_string(),
                    path: RelPath(path.to_string()),
                    definition: format!("{name} の定義"),
                })
                .collect(),
        )
    }

    /// 観点: 本文中の用語出現を全件、位置付きで検出する（CJK は部分一致）。
    #[test]
    fn find_term_mentions_detects_all_occurrences() {
        let g = glossary_of(&[("状態", "glossary/状態.md")]);
        let md = "状態を持つ。次の状態へ遷移する。";
        let ms = find_term_mentions(md, &g, &RelPath("doc.md".into()));
        // "状態" は 2 回出現する。
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].range.start.character, 0);
        // 2 件目の "状態" は char 8（状0 態1 を2 持3 つ4 。5 次6 の7 状8）。
        assert_eq!(ms[1].range.start.character, 8);
    }

    /// 観点: 最長一致優先（"状態遷移" を "状態" より優先して 1 件に）。
    #[test]
    fn find_term_mentions_prefers_longest() {
        let g = glossary_of(&[
            ("状態", "glossary/状態.md"),
            ("状態遷移", "glossary/状態遷移.md"),
        ]);
        let md = "状態遷移について";
        let ms = find_term_mentions(md, &g, &RelPath("doc.md".into()));
        assert_eq!(ms.len(), 1);
        // "状態遷移"（4文字）にマッチしたので、その用語が選ばれている。
        let term = g.get(ms[0].term_index).unwrap();
        assert_eq!(term.name, "状態遷移");
    }

    /// 観点: ASCII 用語は単語境界を要求する（"LIN" は "LINUX" にマッチしない）。
    #[test]
    fn find_term_mentions_ascii_word_boundary() {
        let g = glossary_of(&[("LIN", "glossary/LIN.md")]);
        // "LIN" 単独はマッチ、"LINUX" の中はマッチしない。
        let ms = find_term_mentions("LIN bus and LINUX", &g, &RelPath("doc.md".into()));
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].range.start.character, 0);
    }

    /// 観点: コードブロック・インラインコード・既存リンク内は除外する。
    #[test]
    fn find_term_mentions_excludes_code_and_links() {
        let g = glossary_of(&[("LIN", "glossary/LIN.md")]);
        let md = "LIN を使う。`LIN` は除外。\n\n```\nLIN\n```\n\n[[LIN]] も除外。";
        let ms = find_term_mentions(md, &g, &RelPath("doc.md".into()));
        // 本文先頭の "LIN" 1 件のみ。インラインコード・コードブロック・[[]] は除外。
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].range.start.line, 0);
    }

    /// 観点: 用語自身の定義ノートでは自己リンクしない。
    #[test]
    fn find_term_mentions_skips_self_definition() {
        let g = glossary_of(&[("LIN", "glossary/LIN.md")]);
        let body = "LIN はネットワークプロトコル。";
        // 定義ノート自身（glossary/LIN.md）を走査 → 自己リンクしない。
        let in_self = find_term_mentions(body, &g, &RelPath("glossary/LIN.md".into()));
        assert!(in_self.is_empty());
        // 別ノートでは普通にリンクする。
        let in_other = find_term_mentions(body, &g, &RelPath("doc.md".into()));
        assert_eq!(in_other.len(), 1);
    }

    /// 観点: フロントマターを跨いでも位置（行）が正しく計算される。
    #[test]
    fn find_term_mentions_accounts_for_frontmatter() {
        let g = glossary_of(&[("LIN", "glossary/LIN.md")]);
        let md = "---\ntitle: x\n---\n\nLIN を参照。";
        let ms = find_term_mentions(md, &g, &RelPath("doc.md".into()));
        assert_eq!(ms.len(), 1);
        // フロントマター 3 行 + 空行 = 行 4 に本文。
        assert_eq!(ms[0].range.start.line, 4);
    }

    /// 観点: 空の用語集では何も検出しない（早期 return）。
    #[test]
    fn find_term_mentions_empty_glossary() {
        let g = Glossary::default();
        assert!(find_term_mentions("何か", &g, &RelPath("doc.md".into())).is_empty());
    }

    /// 観点: is_glossary_note が glossary/ 配下のみ true。
    #[test]
    fn is_glossary_note_detects_folder() {
        assert!(is_glossary_note("glossary/LIN.md"));
        assert!(is_glossary_note("glossary/sub/X.md"));
        assert!(!is_glossary_note("docs/glossary.md"));
        assert!(!is_glossary_note("LIN.md"));
    }

    /// 観点: glossary_excerpt はフロントマター・見出し記号を除いた最初の非空行。
    #[test]
    fn glossary_excerpt_extracts_first_line() {
        assert_eq!(
            glossary_excerpt("---\nk: v\n---\n# LIN\n\nLIN は通信規格。"),
            "LIN"
        );
        assert_eq!(glossary_excerpt("\n\n本体の説明"), "本体の説明");
    }

    /// 観点: index_workspace が glossary/ のノートを用語集に取り込む。
    #[test]
    fn workspace_builds_glossary() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_gloss_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("glossary")).unwrap();
        fs::write(
            dir.join("glossary/LIN.md"),
            "# LIN\n\nローカル相互接続網。\n",
        )
        .unwrap();
        fs::write(dir.join("doc.md"), "LIN を採用する。\n").unwrap();

        let idx = index_workspace(&dir);
        assert_eq!(idx.glossary.len(), 1);
        let term = &idx.glossary.terms()[0];
        assert_eq!(term.name, "LIN");
        assert_eq!(term.definition, "LIN");

        // doc.md 側で用語出現が検出できる。
        let ms = find_term_mentions("LIN を採用する。", &idx.glossary, &RelPath("doc.md".into()));
        assert_eq!(ms.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 観点: 用語が出現した doc には Term ノードと Mentions 辺が 1 本追加される。
    /// 同じ用語が複数回出現しても辺は doc×term で 1 本に集約される。
    #[test]
    fn workspace_adds_term_nodes_and_mentions_edges() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_term_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("glossary")).unwrap();
        fs::write(dir.join("glossary/LIN.md"), "LIN は通信規格。\n").unwrap();
        // doc.md に "LIN" が 2 回出現する。辺は 1 本になるはず。
        fs::write(dir.join("doc.md"), "LIN バスは LIN プロトコルを使う。\n").unwrap();

        let idx = index_workspace(&dir);

        // Term ノードが存在する。
        let has_term = idx
            .graph
            .node_ids()
            .any(|n| matches!(n, NodeId::Term(t) if t == "LIN"));
        assert!(has_term, "Term ノードが追加されていること");

        // Mentions 辺がちょうど 1 本。
        let mentions_count = idx
            .graph
            .edges()
            .iter()
            .filter(|e| matches!(e, Edge::Mentions { term, .. } if term == "LIN"))
            .count();
        assert_eq!(
            mentions_count, 1,
            "同一 doc×term の辺は 1 本に集約されること"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 観点: 用語が一度も出現しない場合は Term ノードを作らない（省メモリ）。
    #[test]
    fn workspace_no_term_node_when_no_mention() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_noterm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("glossary")).unwrap();
        fs::write(dir.join("glossary/LIN.md"), "LIN は通信規格。\n").unwrap();
        // doc.md に "LIN" は出現しない。
        fs::write(dir.join("doc.md"), "CAN バスを使う。\n").unwrap();

        let idx = index_workspace(&dir);

        let has_term = idx.graph.node_ids().any(|n| matches!(n, NodeId::Term(_)));
        assert!(!has_term, "出現がない用語は Term ノードを作らないこと");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 観点: 複数の doc から同じ用語が出現したとき、Term ノードは 1 個・辺は doc 数分。
    #[test]
    fn workspace_term_node_deduplicated_across_docs() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_multiterm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("glossary")).unwrap();
        fs::write(dir.join("glossary/LIN.md"), "LIN は通信規格。\n").unwrap();
        fs::write(dir.join("a.md"), "LIN を参照。\n").unwrap();
        fs::write(dir.join("b.md"), "LIN を採用した。\n").unwrap();

        let idx = index_workspace(&dir);

        // Term ノードは重複しない（add_node が排除する）。
        let term_count = idx
            .graph
            .node_ids()
            .filter(|n| matches!(n, NodeId::Term(_)))
            .count();
        assert_eq!(term_count, 1, "Term ノードは 1 個だけ");

        // Mentions 辺は a.md と b.md で 2 本。
        let mentions_count = idx
            .graph
            .edges()
            .iter()
            .filter(|e| matches!(e, Edge::Mentions { .. }))
            .count();
        assert_eq!(mentions_count, 2, "doc 数分の Mentions 辺があること");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 観点: glossary/ 配下に同一ステムが複数ある場合、duplicate_terms に登録される。
    /// 重複がない場合は空集合のままであること。
    #[test]
    fn workspace_detects_duplicate_glossary_terms() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_dup_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("glossary/sub")).unwrap();
        // 同じステム "LIN" を 2 ファイルで作る（別サブディレクトリでも重複と見なす）。
        fs::write(dir.join("glossary/LIN.md"), "LIN は通信規格。\n").unwrap();
        fs::write(dir.join("glossary/sub/LIN.md"), "LIN の別定義。\n").unwrap();
        // "CAN" は 1 ファイルなので重複しない。
        fs::write(dir.join("glossary/CAN.md"), "CAN は別の規格。\n").unwrap();

        let idx = index_workspace(&dir);
        assert!(
            idx.duplicate_terms.contains("LIN"),
            "重複ステム LIN が duplicate_terms に登録されること"
        );
        assert!(
            !idx.duplicate_terms.contains("CAN"),
            "重複なし CAN は duplicate_terms に登録されないこと"
        );
    }

    /// 観点: 重複がない場合は duplicate_terms が空集合。
    #[test]
    fn workspace_no_duplicates_when_unique() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("dbrain_ws_nodup_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("glossary")).unwrap();
        fs::write(dir.join("glossary/LIN.md"), "LIN は通信規格。\n").unwrap();
        fs::write(dir.join("glossary/CAN.md"), "CAN は別の規格。\n").unwrap();

        let idx = index_workspace(&dir);
        assert!(
            idx.duplicate_terms.is_empty(),
            "重複がなければ duplicate_terms は空"
        );
    }
}
