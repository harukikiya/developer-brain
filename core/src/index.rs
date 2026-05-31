//! # 索引器（M1）: Markdown を知識グラフに変換する
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

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use serde::Serialize;

use crate::code::{extract_symbols, SymbolTable};
use crate::model::{DocTarget, Edge, NodeId, Reference, RelPath, Resolution, SymbolDescriptor};

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
    pub fn to_json_string(&self) -> String {
        let out = GraphJson {
            nodes: self
                .nodes
                .iter()
                .map(|n| NodeJson {
                    id: node_id_string(&n.id),
                    kind: node_kind(&n.id),
                    label: n.label.clone(),
                })
                .collect(),
            edges: self.edges.iter().map(edge_json).collect(),
        };
        serde_json::to_string(&out).unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[]}".to_string())
    }
}

/// ワークスペースを走査して知識グラフを組み立てる。
pub fn index_workspace(root: &Path) -> KnowledgeGraph {
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
    let mut docs: Vec<(RelPath, ParsedDoc)> = Vec::new();
    for path in walk_by_ext(root, "md") {
        let rel = to_rel(root, &path);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        docs.push((rel, index_document(&content)));
    }

    let mut doc_paths: HashSet<String> = HashSet::new();
    // 短縮名（ファイル名ステム）→ パス。一意なものだけ解決に使う。
    let mut stem_to_path: HashMap<String, Option<RelPath>> = HashMap::new();

    for (rel, parsed) in &docs {
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
    for (rel, parsed) in &docs {
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

    graph
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
        let masked = extract_wikilinks(text, &[idx..idx + 10]);
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
        let json = g.to_json_string();
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

        let g = index_workspace(&dir);
        assert_eq!(g.code_refs_resolved, 1);
        assert_eq!(g.code_refs_unresolved, 0);

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

        let g = index_workspace(&dir);
        assert_eq!(g.code_refs_resolved, 0);
        assert_eq!(g.code_refs_unresolved, 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
