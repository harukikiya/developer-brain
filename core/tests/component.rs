//! # コンポーネントテスト（index_workspace を中心とした結合動作）
//!
//! 単体テスト（各 `src/*.rs` 内）が「関数単位の正しさ」を見るのに対し、ここでは
//! **公開 API だけ** を使って「部品を組み合わせた振る舞い」を観点別に検証する。
//!
//! ## テスト観点マトリクス
//!
//! | # | 観点                     | 入力条件                                   | 期待                                       | テスト関数 |
//! |---|--------------------------|--------------------------------------------|--------------------------------------------|-----------|
//! | 1 | Rust シンボル抽出         | `.rs` に fn / struct / impl                | Symbol ノードが生成される                  | `vp1_rust_symbols` |
//! | 2 | C シンボル抽出            | `.c` に 関数 / グローバル / struct          | Symbol ノードが生成される                  | `vp2_c_symbols` |
//! | 3 | doc→code 解決            | `.md` の `[[f.rs@Sym]]` が実在              | References 辺・resolved=1                   | `vp3_doc_to_code_resolved` |
//! | 4 | dangling                 | `[[f.rs@Missing]]`                          | 辺なし・unresolved=1                        | `vp4_dangling` |
//! | 5 | doc→doc / 短縮名         | `[[other.md]]` と `[[stem]]`                | Links 辺が 2 本                             | `vp5_doc_links_and_shortname` |
//! | 6 | 行ズレ耐性（設計の核心）  | 同じコードの前に空行を挿入して再索引        | 同じ参照が解決し、range が挿入行数ぶん移動  | `vp6_line_shift_stability` |
//! | 7 | 頑健性（除外）            | frontmatter / コードブロック内 `[[]]`       | 見出し誤検出なし・コード内はリンク化しない  | `vp7_robustness_exclusions` |
//! | 8 | 混在ワークスペース        | `.rs` + `.c` + `.md` 同居                    | 1 グラフに統合・両言語の参照が解決          | `vp8_mixed_workspace` |
//! | 9 | メンバ粒度（C）          | フィールド `[[c@Tag::field]]` / 列挙子 `[[c@CONST]]` | どちらも解決・resolved=2             | `vp9_c_member_granularity` |
//! | 10| メンバ粒度（Rust）       | フィールド `[[rs@Type::field]]` / バリアント `[[rs@Enum::Variant]]` | どちらも解決・resolved=2 | `vp10_rust_member_granularity` |

use std::fs;
use std::path::{Path, PathBuf};

use dbrain_core::code::{extract_rust_symbols, SymbolTable};
use dbrain_core::index::{index_document, index_workspace, KnowledgeGraph};
use dbrain_core::model::{
    DocTarget, Edge, NodeId, Reference, RelPath, Resolution, SymbolDescriptor, SymbolPath,
};

// --- 一時ワークスペース（Drop で自動削除） ---

struct TempWs {
    dir: PathBuf,
}

impl TempWs {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dbrain_comp_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempWs { dir }
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// 述語に合致するエッジの本数を数える小道具。
fn count_edges(g: &KnowledgeGraph, pred: impl Fn(&Edge) -> bool) -> usize {
    g.edges().iter().filter(|e| pred(e)).count()
}

/// Symbol ノードの数を数える小道具。
fn symbol_nodes(g: &KnowledgeGraph) -> usize {
    g.node_ids()
        .filter(|n| matches!(n, NodeId::Symbol(_)))
        .count()
}

// --- 観点 1: Rust シンボル抽出 ---

#[test]
fn vp1_rust_symbols() {
    let ws = TempWs::new("vp1");
    ws.write("lib.rs", "pub struct Foo;\nimpl Foo { pub fn bar() {} }\n");
    let idx = index_workspace(ws.path());
    // Foo（struct）と Foo::bar（method）の 2 つは少なくとも出る。
    assert!(
        symbol_nodes(&idx.graph) >= 2,
        "expected >=2 symbols, got {}",
        symbol_nodes(&idx.graph)
    );
}

// --- 観点 2: C シンボル抽出 ---

#[test]
fn vp2_c_symbols() {
    let ws = TempWs::new("vp2");
    ws.write(
        "mod.c",
        "static int g_count;\nstruct Point { int x; };\nvoid run(void) {}\n",
    );
    let idx = index_workspace(ws.path());
    // g_count（Static）/ Point（Struct）/ run（Function）の 3 つ。
    assert!(
        symbol_nodes(&idx.graph) >= 3,
        "expected >=3 C symbols, got {}",
        symbol_nodes(&idx.graph)
    );
}

// --- 観点 3: doc→code 解決 ---

#[test]
fn vp3_doc_to_code_resolved() {
    let ws = TempWs::new("vp3");
    ws.write("lib.rs", "pub struct Foo;\nimpl Foo { pub fn bar() {} }\n");
    ws.write("doc.md", "詳細は [[lib.rs@Foo::bar]] を参照。\n");
    let idx = index_workspace(ws.path());
    assert_eq!(idx.graph.code_refs_resolved, 1);
    assert_eq!(idx.graph.code_refs_unresolved, 0);
    assert_eq!(
        count_edges(&idx.graph, |e| matches!(e, Edge::References { .. })),
        1
    );
}

// --- 観点 4: dangling（解決不能） ---

#[test]
fn vp4_dangling() {
    let ws = TempWs::new("vp4");
    ws.write("lib.rs", "pub fn exists() {}\n");
    ws.write("doc.md", "[[lib.rs@does_not_exist]]\n");
    let idx = index_workspace(ws.path());
    assert_eq!(idx.graph.code_refs_resolved, 0);
    assert_eq!(idx.graph.code_refs_unresolved, 1);
    assert_eq!(
        count_edges(&idx.graph, |e| matches!(e, Edge::References { .. })),
        0
    );
}

// --- 観点 5: doc→doc リンクと短縮名 ---

#[test]
fn vp5_doc_links_and_shortname() {
    let ws = TempWs::new("vp5");
    // a.md からフルパス [[b.md]] と短縮名 [[b]] の両方で b.md を指す。
    ws.write("a.md", "見る: [[b.md]] と [[b]]\n");
    ws.write("b.md", "# B\n");
    let idx = index_workspace(ws.path());
    // 2 本とも b.md に解決し、Links 辺が 2 本張られる。
    assert_eq!(
        count_edges(&idx.graph, |e| matches!(e, Edge::Links { .. })),
        2
    );
}

// --- 観点 6: 行ズレ耐性（このツールの設計の核心） ---

#[test]
fn vp6_line_shift_stability() {
    // 「参照は座標ではなくクエリ」の検証。同じシンボルを、コードの前に空行を
    // 入れた版で索引し直しても、同じ記述子で解決でき、range だけが移動する。
    let file = RelPath("x.rs".into());
    let desc = SymbolDescriptor {
        file: file.clone(),
        path: SymbolPath(vec!["Foo".into(), "bar".into()]),
        kind: None,
        disambiguator: None,
    };

    let v1 = "impl Foo { fn bar() {} }\n";
    let shift = 3; // 前に 3 行の空行を足す
    let v2 = format!("\n\n\n{v1}");

    let line_of = |src: &str| -> u32 {
        let mut t = SymbolTable::default();
        for e in extract_rust_symbols(&file, src) {
            t.intern(e);
        }
        match t.resolve(&desc) {
            Resolution::Resolved { range, .. } => range.start.line,
            other => panic!("expected Resolved, got {other:?}"),
        }
    };

    let line1 = line_of(v1);
    let line2 = line_of(&v2);
    // 参照（記述子）は不変のまま、解決される位置だけが挿入行数ぶん下がる。
    assert_eq!(line2, line1 + shift, "リンク先の行が挿入ぶん追従するはず");
}

// --- 観点 7: 頑健性（フロントマター・コード除外） ---

#[test]
fn vp7_robustness_exclusions() {
    // フロントマターは見出しにせず、コードブロック/インラインコード内の
    // [[...]] はリンク化しない。
    let md = "\
---
title: メモ
---

# 実見出し

本文 [[real.md]]

```
[[fenced.md]]
```

インライン `[[inline.md]]` 終わり
";
    let parsed = index_document(md);
    assert_eq!(parsed.headings, vec!["実見出し".to_string()]);
    assert_eq!(parsed.refs.len(), 1);
    match &parsed.refs[0] {
        Reference::Doc {
            target: DocTarget::Path(rp),
            ..
        } => assert_eq!(rp.0, "real.md"),
        other => panic!("expected only real.md, got {other:?}"),
    }
}

// --- 観点 8: 混在ワークスペース ---

#[test]
fn vp8_mixed_workspace() {
    let ws = TempWs::new("vp8");
    ws.write("lib.rs", "pub fn rust_fn() {}\n");
    ws.write("driver.c", "void c_fn(void) {}\n");
    ws.write(
        "spec.md",
        "Rust 側 [[lib.rs@rust_fn]] と C 側 [[driver.c@c_fn]] を参照。\n",
    );
    let idx = index_workspace(ws.path());

    // 両言語のシンボルが 1 つのグラフに同居し、両方の参照が解決する。
    assert!(symbol_nodes(&idx.graph) >= 2);
    assert_eq!(idx.graph.code_refs_resolved, 2);
    assert_eq!(
        count_edges(&idx.graph, |e| matches!(e, Edge::References { .. })),
        2
    );

    // doc ノードと section ノードも含まれる（包含関係の確認）。
    assert!(idx.graph.node_ids().any(|n| matches!(n, NodeId::Doc(_))));
}

// --- 観点 9: C のメンバ粒度（フィールド・列挙子の解決） ---

#[test]
fn vp9_c_member_granularity() {
    let ws = TempWs::new("vp9");
    ws.write(
        "geo.c",
        "struct Point { int x; int y; };\nenum Color { RED, GREEN };\n",
    );
    // フィールドは Tag::field、列挙子は C のグローバルに忠実に単独名で参照する。
    ws.write("doc.md", "座標 [[geo.c@Point::x]] と色 [[geo.c@RED]]。\n");
    let idx = index_workspace(ws.path());
    assert_eq!(idx.graph.code_refs_resolved, 2);
    assert_eq!(idx.graph.code_refs_unresolved, 0);
    assert_eq!(
        count_edges(&idx.graph, |e| matches!(e, Edge::References { .. })),
        2
    );
}

// --- 観点 10: Rust のメンバ粒度（フィールド・バリアントの解決） ---

#[test]
fn vp10_rust_member_granularity() {
    let ws = TempWs::new("vp10");
    ws.write(
        "cfg.rs",
        "pub struct Config { pub timeout: u32 }\npub enum State { Idle, Running }\n",
    );
    // Rust はフィールドもバリアントも型にスコープされるので 2 段で参照する。
    ws.write(
        "doc.md",
        "設定 [[cfg.rs@Config::timeout]] と状態 [[cfg.rs@State::Idle]]。\n",
    );
    let idx = index_workspace(ws.path());
    assert_eq!(idx.graph.code_refs_resolved, 2);
    assert_eq!(idx.graph.code_refs_unresolved, 0);
    assert_eq!(
        count_edges(&idx.graph, |e| matches!(e, Edge::References { .. })),
        2
    );
}
