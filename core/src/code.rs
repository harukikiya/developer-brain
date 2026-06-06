//! # コードシンボル索引（M1）: tree-sitter で Rust / C を解析
//!
//! 「参照は座標ではなくクエリ」（`docs/DESIGN.md` D3）を実現する中核。
//! ソースを tree-sitter で構文解析し、**定義シンボル**（関数・構造体・
//! メソッド等）を、スコープを辿った修飾名（[`SymbolPath`]）付きで取り出す。
//! 行番号は記述子に保存せず、毎回ここで解決し直す。
//!
//! 言語は [`extract_symbols`] が拡張子でディスパッチする（`.rs`→Rust /
//! `.c`,`.h`→C）。Rust はスコープ（mod/impl/trait）を辿って修飾名を組み立て、
//! C は名前空間が無いので名前 1 段（D6）。

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::model::{
    Position, Range, RelPath, Resolution, SymbolDescriptor, SymbolEntry, SymbolId, SymbolKind,
    SymbolPath,
};

/// スコープスタックの 1 段。`type_scope` が真なら、その中の関数はメソッド扱い。
struct Frame {
    name: String,
    type_scope: bool,
}

/// Rust ソースから定義シンボルを抽出する **純粋関数**（ファイルに触れない）。
pub fn extract_rust_symbols(file: &RelPath, source: &str) -> Vec<SymbolEntry> {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_rust::language()).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    let mut scope: Vec<Frame> = Vec::new();
    collect(tree.root_node(), source, file, &mut scope, &mut out);
    out
}

/// 木を再帰的に降りながら定義を集める。スコープに入る節（mod/impl/trait）では
/// 名前を積んでから子を辿る。関数本体には降りない（ネスト定義は対象外）。
fn collect(
    node: Node,
    src: &str,
    file: &RelPath,
    scope: &mut Vec<Frame>,
    out: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // 関数（impl/trait の中ならメソッド）。本体には降りない。
            "function_item" | "function_signature_item" => {
                if let Some((name, name_node)) = field_name(child, "name", src) {
                    let is_method = scope.last().map(|f| f.type_scope).unwrap_or(false);
                    let kind = if is_method {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    push(out, file, scope, &name, kind, child, name_node, src);
                }
            }
            // フィールド/バリアントには降りない（Field は将来対応）。
            // struct/union: 型を積み、名前付きフィールドを Field として拾う。
            "struct_item" | "union_item" => {
                if let Some((name, name_node)) = field_name(child, "name", src) {
                    push(
                        out,
                        file,
                        scope,
                        &name,
                        SymbolKind::Struct,
                        child,
                        name_node,
                        src,
                    );
                    scope.push(Frame {
                        name,
                        type_scope: false,
                    });
                    emit_rust_fields(child, src, file, scope, out);
                    scope.pop();
                }
            }
            // enum: 型を積み、バリアントを Variant として拾う。
            "enum_item" => {
                if let Some((name, name_node)) = field_name(child, "name", src) {
                    push(
                        out,
                        file,
                        scope,
                        &name,
                        SymbolKind::Enum,
                        child,
                        name_node,
                        src,
                    );
                    scope.push(Frame {
                        name,
                        type_scope: false,
                    });
                    emit_rust_variants(child, src, file, scope, out);
                    scope.pop();
                }
            }
            "const_item" => emit(child, src, file, scope, SymbolKind::Const, out),
            "static_item" => emit(child, src, file, scope, SymbolKind::Static, out),
            "type_item" => emit(child, src, file, scope, SymbolKind::Type, out),
            "macro_definition" => emit(child, src, file, scope, SymbolKind::Macro, out),

            // trait は記号でもありメソッド群のスコープでもある。
            "trait_item" => {
                if let Some((name, name_node)) = field_name(child, "name", src) {
                    push(
                        out,
                        file,
                        scope,
                        &name,
                        SymbolKind::Trait,
                        child,
                        name_node,
                        src,
                    );
                    scope.push(Frame {
                        name,
                        type_scope: true,
                    });
                    collect(child, src, file, scope, out);
                    scope.pop();
                }
            }
            // mod は記号かつ（型でない）スコープ。
            "mod_item" => {
                if let Some((name, name_node)) = field_name(child, "name", src) {
                    push(
                        out,
                        file,
                        scope,
                        &name,
                        SymbolKind::Module,
                        child,
                        name_node,
                        src,
                    );
                    scope.push(Frame {
                        name,
                        type_scope: false,
                    });
                    collect(child, src, file, scope, out);
                    scope.pop();
                }
            }
            // impl 自体は記号でない。型名を積んでメソッドのスコープにする。
            "impl_item" => {
                let ty = impl_type_name(child, src).unwrap_or_else(|| "<impl>".to_string());
                scope.push(Frame {
                    name: ty,
                    type_scope: true,
                });
                collect(child, src, file, scope, out);
                scope.pop();
            }

            // それ以外（declaration_list 等の包み）は素通りで降りる。
            _ => collect(child, src, file, scope, out),
        }
    }
}

/// `name` フィールドを持つ単純な定義を 1 件積むヘルパ。
fn emit(
    child: Node,
    src: &str,
    file: &RelPath,
    scope: &[Frame],
    kind: SymbolKind,
    out: &mut Vec<SymbolEntry>,
) {
    if let Some((name, name_node)) = field_name(child, "name", src) {
        push(out, file, scope, &name, kind, child, name_node, src);
    }
}

/// struct/union 本体の名前付きフィールドを Field として拾う。
/// 呼び出し時点で scope に型名が積まれているので、修飾名は `[..., 型名, フィールド名]`。
/// タプル構造体（名前無し）はスキップ。
fn emit_rust_fields(
    struct_node: Node,
    src: &str,
    file: &RelPath,
    scope: &[Frame],
    out: &mut Vec<SymbolEntry>,
) {
    let body = match struct_node.child_by_field_name("body") {
        Some(b) if b.kind() == "field_declaration_list" => b,
        _ => return,
    };
    let mut cursor = body.walk();
    for f in body.children(&mut cursor) {
        if f.kind() == "field_declaration" {
            if let Some((name, name_node)) = field_name(f, "name", src) {
                push(
                    out,
                    file,
                    scope,
                    &name,
                    SymbolKind::Field,
                    f,
                    name_node,
                    src,
                );
            }
        }
    }
}

/// enum 本体のバリアントを Variant として拾う（修飾名は `[..., enum 名, バリアント名]`）。
fn emit_rust_variants(
    enum_node: Node,
    src: &str,
    file: &RelPath,
    scope: &[Frame],
    out: &mut Vec<SymbolEntry>,
) {
    let body = match enum_node.child_by_field_name("body") {
        Some(b) => b,
        None => return,
    };
    let mut cursor = body.walk();
    for v in body.children(&mut cursor) {
        if v.kind() == "enum_variant" {
            if let Some((name, name_node)) = field_name(v, "name", src) {
                push(
                    out,
                    file,
                    scope,
                    &name,
                    SymbolKind::Variant,
                    v,
                    name_node,
                    src,
                );
            }
        }
    }
}

/// 記述子を組み立てて 1 件追加する。path = スコープ名 + 自分の名前。
// `src` の追加で引数が 8 個になるが、単純な内部ヘルパへの許容範囲として抑制する。
#[allow(clippy::too_many_arguments)]
fn push(
    out: &mut Vec<SymbolEntry>,
    file: &RelPath,
    scope: &[Frame],
    name: &str,
    kind: SymbolKind,
    def: Node,
    name_node: Node,
    src: &str,
) {
    let mut path: Vec<String> = scope.iter().map(|f| f.name.clone()).collect();
    path.push(name.to_string());
    out.push(SymbolEntry {
        descriptor: SymbolDescriptor {
            file: file.clone(),
            path: SymbolPath(path),
            kind: Some(kind),
            disambiguator: None,
        },
        range: node_range(def, src),
        name_range: node_range(name_node, src),
    });
}

/// `node` の `field` 子ノードの名前テキストとノードを取り出す。
fn field_name<'a>(node: Node<'a>, field: &str, src: &str) -> Option<(String, Node<'a>)> {
    let n = node.child_by_field_name(field)?;
    let text = n.utf8_text(src.as_bytes()).ok()?.to_string();
    Some((text, n))
}

/// `impl Foo<T>` や `impl path::Foo` から基底の型名 `Foo` を取り出す。
fn impl_type_name(impl_node: Node, src: &str) -> Option<String> {
    let ty = impl_node.child_by_field_name("type")?;
    let text = ty.utf8_text(src.as_bytes()).ok()?;
    let base = text.split('<').next().unwrap_or(text).trim();
    let last = base.rsplit("::").next().unwrap_or(base).trim();
    Some(last.to_string())
}

/// tree-sitter の位置（0 始まり）を [`Range`] に変換する。
///
/// tree-sitter の `column` はバイト列だが、[`model::Position::character`] は
/// UTF-8 コードポイント数（char 数）で統一する。ASCII 識別子では両者は一致するが、
/// 日本語識別子（`#[allow(non_ascii_idents)]`）があると食い違うため、ここで変換する。
/// LSP が要求する UTF-16 への変換は LSP アダプタ層の責務（[`dbrain_lsp`] 側で行う）。
fn node_range(node: Node, src: &str) -> Range {
    let s = node.start_position();
    let e = node.end_position();
    Range {
        start: Position {
            line: s.row as u32,
            character: byte_col_to_char_count(src, s.row, s.column),
        },
        end: Position {
            line: e.row as u32,
            character: byte_col_to_char_count(src, e.row, e.column),
        },
    }
}

/// tree-sitter のバイト列（`column`）を UTF-8 char 数に変換するヘルパ。
///
/// `src` の `row` 行目の先頭から `byte_col` バイト目までに含まれる Unicode
/// スカラー値の個数を返す。`byte_col` が行末を超える場合は行末で打ち切る。
fn byte_col_to_char_count(src: &str, row: usize, byte_col: usize) -> u32 {
    let line = src.lines().nth(row).unwrap_or("");
    line[..byte_col.min(line.len())].chars().count() as u32
}

// ---------------------------------------------------------------------------
// C のシンボル抽出
// ---------------------------------------------------------------------------
//
// C は名前空間が無い（`docs/DESIGN.md` D6）ので、修飾名 [`SymbolPath`] は
// 常に 1 段（関数名・型名そのもの）。スコープスタックは不要。
// 一方で C は宣言子（declarator）が入れ子になる（ポインタ・配列・関数ポインタ）
// ため、「宣言子の一番内側にある識別子」を取り出す再帰ヘルパが要る。

/// 拡張子でディスパッチして、対応言語のシンボルを抽出する公開入口。
pub fn extract_symbols(file: &RelPath, source: &str) -> Vec<SymbolEntry> {
    match ext_of(&file.0) {
        Some("rs") => extract_rust_symbols(file, source),
        Some("c") | Some("h") => extract_c_symbols(file, source),
        _ => Vec::new(),
    }
}

/// パス文字列の拡張子（小文字化はしない）。
fn ext_of(path: &str) -> Option<&str> {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
}

/// C ソースから定義シンボルを抽出する純粋関数。
pub fn extract_c_symbols(file: &RelPath, source: &str) -> Vec<SymbolEntry> {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_c::language()).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    collect_c(tree.root_node(), source, file, &mut out);
    out
}

/// C の木を辿って定義を集める。関数本体（compound_statement）には降りない。
fn collect_c(node: Node, src: &str, file: &RelPath, out: &mut Vec<SymbolEntry>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // 関数定義。名前は declarator の最内 identifier。本体には降りない。
            "function_definition" => {
                if let Some(decl) = child.child_by_field_name("declarator") {
                    if let Some((name, name_node)) = declarator_name(decl, src) {
                        push_c(
                            out,
                            file,
                            &name,
                            SymbolKind::Function,
                            child,
                            name_node,
                            src,
                        );
                    }
                }
            }
            // #define（オブジェクト形式・関数形式とも）→ Macro。
            "preproc_def" | "preproc_function_def" => {
                if let Some((name, name_node)) = field_name(child, "name", src) {
                    push_c(out, file, &name, SymbolKind::Macro, child, name_node, src);
                }
            }
            // typedef → 新しい型名を Type に。型部に struct/enum 定義があれば併せて拾う。
            "type_definition" => emit_c_typedef(child, src, file, out),
            // 宣言：グローバル変数、または型部の struct/enum 定義。
            "declaration" => emit_c_declaration(child, src, file, out),
            // 素の struct/union/enum 定義（宣言で包まれない稀なケース）。
            "struct_specifier" | "union_specifier" | "enum_specifier" => {
                emit_c_tagged(child, src, file, out)
            }
            // 容器（プリプロセッサ条件・外部リンケージ・宣言リスト）は中を辿る。
            "linkage_specification"
            | "preproc_if"
            | "preproc_ifdef"
            | "preproc_else"
            | "preproc_elif"
            | "declaration_list" => collect_c(child, src, file, out),
            _ => {}
        }
    }
}

/// 名前と本体を持つ struct/union/enum 定義を積む（前方宣言・参照は除く）。
/// さらにメンバも拾う: struct/union のフィールド（`[Tag, field]` の Field）、
/// enum の列挙子（C ではグローバルなので `[CONST]` の Const）。
fn emit_c_tagged(node: Node, src: &str, file: &RelPath, out: &mut Vec<SymbolEntry>) {
    let body = match node.child_by_field_name("body") {
        Some(b) => b,
        None => return, // 本体が無い＝定義ではない（前方宣言や型参照）
    };
    let (tag_name, name_node) = match field_name(node, "name", src) {
        Some(v) => v,
        None => return, // 無名 struct/enum はタグが無いのでここでは積まない
    };

    let is_enum = node.kind() == "enum_specifier";
    let kind = if is_enum {
        SymbolKind::Enum
    } else {
        SymbolKind::Struct
    };
    push_c(out, file, &tag_name, kind, node, name_node, src);

    // メンバを拾う。
    if is_enum {
        emit_enum_constants(body, src, file, out);
    } else {
        emit_struct_fields(body, &tag_name, src, file, out);
    }
}

/// struct/union 本体（field_declaration_list）からフィールドを Field として拾う。
/// 修飾名は `[タグ名, フィールド名]`（フィールドはその型に属するため）。
fn emit_struct_fields(
    body: Node,
    tag: &str,
    src: &str,
    file: &RelPath,
    out: &mut Vec<SymbolEntry>,
) {
    let mut cursor = body.walk();
    for field in body.children(&mut cursor) {
        if field.kind() != "field_declaration" {
            continue; // 匿名 union やコメント等は飛ばす
        }
        // 1 つの宣言に複数フィールド（`int x, y;`）があり得るので全宣言子を走査。
        let mut c2 = field.walk();
        for d in field.children(&mut c2) {
            let declarator = match d.kind() {
                "field_identifier" | "pointer_declarator" | "array_declarator" => Some(d),
                _ => None,
            };
            if let Some(dn) = declarator {
                if let Some((name, name_node)) = declarator_name(dn, src) {
                    push_c_path(
                        out,
                        file,
                        vec![tag.to_string(), name],
                        SymbolKind::Field,
                        field,
                        name_node,
                        src,
                    );
                }
            }
        }
    }
}

/// enum 本体（enumerator_list）から列挙子を Const として拾う。
/// C の列挙子はグローバルスコープなので修飾名は 1 段。
fn emit_enum_constants(body: Node, src: &str, file: &RelPath, out: &mut Vec<SymbolEntry>) {
    let mut cursor = body.walk();
    for e in body.children(&mut cursor) {
        if e.kind() == "enumerator" {
            if let Some((name, name_node)) = field_name(e, "name", src) {
                push_c(out, file, &name, SymbolKind::Const, e, name_node, src);
            }
        }
    }
}

/// グローバル宣言から、変数（Variable/Static）と型部の struct/enum 定義を拾う。
fn emit_c_declaration(node: Node, src: &str, file: &RelPath, out: &mut Vec<SymbolEntry>) {
    let type_node = node.child_by_field_name("type");
    if let Some(ty) = type_node {
        if matches!(
            ty.kind(),
            "struct_specifier" | "union_specifier" | "enum_specifier"
        ) {
            emit_c_tagged(ty, src, file, out);
        }
    }

    let is_static = has_static_storage(node, src);
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        // 宣言子だけを対象にする（型・記憶域指定子・型部の struct 定義は除外）。
        let declarator = match c.kind() {
            "init_declarator" => c.child_by_field_name("declarator"),
            "identifier" | "pointer_declarator" | "array_declarator" | "function_declarator" => {
                Some(c)
            }
            _ => None,
        };
        if let Some(d) = declarator {
            if is_prototype(d) {
                continue; // 関数プロトタイプは function_definition 側で扱う
            }
            if let Some((name, name_node)) = declarator_name(d, src) {
                let kind = if is_static {
                    SymbolKind::Static
                } else {
                    SymbolKind::Variable
                };
                push_c(out, file, &name, kind, node, name_node, src);
            }
        }
    }
}

/// typedef から新しい型名（と型部の struct/enum 定義）を拾う。
fn emit_c_typedef(node: Node, src: &str, file: &RelPath, out: &mut Vec<SymbolEntry>) {
    let type_node = node.child_by_field_name("type");
    if let Some(ty) = type_node {
        if matches!(
            ty.kind(),
            "struct_specifier" | "union_specifier" | "enum_specifier"
        ) {
            emit_c_tagged(ty, src, file, out);
        }
    }

    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        // 既存型（type フィールド）は新しい名前ではないので飛ばす。
        if Some(c.id()) == type_node.map(|t| t.id()) {
            continue;
        }
        if let Some((name, name_node)) = declarator_name(c, src) {
            push_c(out, file, &name, SymbolKind::Type, node, name_node, src);
        }
    }
}

/// 宣言子が「関数プロトタイプ」か（`int foo(void);` 形）。
/// 関数ポインタ変数 `int (*fp)(void);` は内側が identifier でないので false。
fn is_prototype(d: Node) -> bool {
    d.kind() == "function_declarator"
        && d.child_by_field_name("declarator")
            .map(|i| i.kind() == "identifier")
            .unwrap_or(false)
}

/// 宣言に `static` 記憶域指定子が付いているか。
fn has_static_storage(node: Node, src: &str) -> bool {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if c.kind() == "storage_class_specifier"
            && c.utf8_text(src.as_bytes())
                .map(|t| t.trim() == "static")
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// 入れ子の宣言子（ポインタ・配列・関数・括弧）を辿って最内の識別子を得る。
fn declarator_name<'a>(node: Node<'a>, src: &str) -> Option<(String, Node<'a>)> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" => {
            Some((node.utf8_text(src.as_bytes()).ok()?.to_string(), node))
        }
        "pointer_declarator"
        | "array_declarator"
        | "function_declarator"
        | "init_declarator"
        | "parenthesized_declarator" => {
            declarator_name(node.child_by_field_name("declarator")?, src)
        }
        _ => None,
    }
}

/// C 用に記号を 1 件積む（単一段の名前）。
fn push_c(
    out: &mut Vec<SymbolEntry>,
    file: &RelPath,
    name: &str,
    kind: SymbolKind,
    def: Node,
    name_node: Node,
    src: &str,
) {
    push_c_path(out, file, vec![name.to_string()], kind, def, name_node, src);
}

/// 任意段の修飾名で記号を 1 件積む（フィールドの `[Tag, field]` 等）。
fn push_c_path(
    out: &mut Vec<SymbolEntry>,
    file: &RelPath,
    path: Vec<String>,
    kind: SymbolKind,
    def: Node,
    name_node: Node,
    src: &str,
) {
    out.push(SymbolEntry {
        descriptor: SymbolDescriptor {
            file: file.clone(),
            path: SymbolPath(path),
            kind: Some(kind),
            disambiguator: None,
        },
        range: node_range(def, src),
        name_range: node_range(name_node, src),
    });
}

// ---------------------------------------------------------------------------
// シンボル表（intern と解決）
// ---------------------------------------------------------------------------

/// 抽出したシンボルを保持し、参照を解決する表。
///
/// 重い [`SymbolDescriptor`] はここに 1 コピーだけ持ち、グラフ側は軽い
/// [`SymbolId`] で参照する（省メモリ。`CLAUDE.md` の不変条件 4）。
#[derive(Default)]
pub struct SymbolTable {
    entries: Vec<SymbolEntry>,
    /// (ファイル, 修飾名) → 候補 ID 群。同名衝突（オーバーロード）は複数になる。
    by_key: HashMap<(RelPath, SymbolPath), Vec<SymbolId>>,
}

impl SymbolTable {
    /// 1 件登録し、その [`SymbolId`] を返す。
    pub fn intern(&mut self, entry: SymbolEntry) -> SymbolId {
        let id = SymbolId(self.entries.len() as u32);
        let key = (entry.descriptor.file.clone(), entry.descriptor.path.clone());
        self.by_key.entry(key).or_default().push(id);
        self.entries.push(entry);
        id
    }

    /// 参照記述子を解決する。種別やヒントは見ず、ファイル＋修飾名で突き合わせる。
    pub fn resolve(&self, desc: &SymbolDescriptor) -> Resolution {
        match self.by_key.get(&(desc.file.clone(), desc.path.clone())) {
            None => Resolution::Dangling,
            Some(ids) if ids.len() == 1 => Resolution::Resolved {
                id: ids[0],
                range: self.entries[ids[0].0 as usize].range,
            },
            Some(ids) => Resolution::Ambiguous(ids.clone()),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// ID からシンボルエントリを取得する。
    ///
    /// GotoDefinition で `name_range`（名前部分だけの位置）を参照するときに使う。
    /// 定義全体ではなく名前の先頭にカーソルを置きたいため、`range` ではなく
    /// `name_range` を使うのが精度の良い実装になる。
    pub fn get(&self, id: SymbolId) -> Option<&SymbolEntry> {
        self.entries.get(id.0 as usize)
    }

    /// 登録済みシンボルを (ID, 実体) で走査する。
    pub fn iter(&self) -> impl Iterator<Item = (SymbolId, &SymbolEntry)> {
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e)| (SymbolId(i as u32), e))
    }
}

// ===========================================================================
// 単体テスト
// ===========================================================================
//
// 方針:
// - 公開関数（extract_symbols / extract_rust_symbols / extract_c_symbols /
//   SymbolTable の各メソッド）を直接テストする。
// - private ヘルパ（collect / push / field_name / impl_type_name /
//   declarator_name / node_range / ext_of 等）は、Node を手で作るのが現実的で
//   ないため、**公開呼び出し経由で間接的に**検証する。どのテストがどの
//   ヘルパを担保しているかは各テストのコメントに明記する。
#[cfg(test)]
mod tests {
    use super::*;

    /// 抽出結果から「修飾名（`::` 連結）」の一覧を作る小道具。
    fn paths(syms: &[SymbolEntry]) -> Vec<String> {
        syms.iter()
            .map(|e| e.descriptor.path.0.join("::"))
            .collect()
    }

    /// 指定の修飾名を持つシンボルを 1 件取り出す小道具。
    fn find<'a>(syms: &'a [SymbolEntry], qualified: &str) -> &'a SymbolEntry {
        syms.iter()
            .find(|e| e.descriptor.path.0.join("::") == qualified)
            .unwrap_or_else(|| panic!("symbol not found: {qualified}"))
    }

    // ----- Rust: 修飾名の再構成（collect / push / field_name / impl_type_name を間接検証） -----

    /// 観点: 関数・構造体・メソッド・モジュール・モジュール内関数が、
    /// 正しい修飾名で取れること。impl 内の関数が `Foo::method` になること＝
    /// スコープスタックと impl_type_name が効いている証拠。
    #[test]
    fn rust_reconstructs_qualified_names() {
        let src = "\
struct Foo;
fn free() {}
impl Foo { fn method(&self) {} }
mod m { fn inner() {} }
";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        let p = paths(&syms);
        assert!(p.contains(&"Foo".to_string()));
        assert!(p.contains(&"free".to_string()));
        assert!(p.contains(&"Foo::method".to_string()));
        assert!(p.contains(&"m".to_string()));
        assert!(p.contains(&"m::inner".to_string()));
    }

    /// 観点: 種別（Function/Method/Struct/Module）が正しく付くこと。
    /// impl 内＝Method、トップレベル＝Function の判定（type_scope フラグ）。
    #[test]
    fn rust_assigns_correct_kinds() {
        let src = "struct Foo;\nfn free() {}\nimpl Foo { fn method() {} }\nmod m {}\n";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        assert_eq!(
            find(&syms, "free").descriptor.kind,
            Some(SymbolKind::Function)
        );
        assert_eq!(
            find(&syms, "Foo::method").descriptor.kind,
            Some(SymbolKind::Method)
        );
        assert_eq!(find(&syms, "Foo").descriptor.kind, Some(SymbolKind::Struct));
        assert_eq!(find(&syms, "m").descriptor.kind, Some(SymbolKind::Module));
    }

    /// 観点: enum / trait（とトレイトのメソッドシグネチャ）/ const / static /
    /// type エイリアス / マクロ がそれぞれ拾えること。
    #[test]
    fn rust_extracts_all_item_kinds() {
        let src = "\
enum E { A, B }
trait T { fn req(&self); }
const C: u8 = 0;
static S: u8 = 1;
type Alias = u8;
macro_rules! mac { () => {} }
";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        assert_eq!(find(&syms, "E").descriptor.kind, Some(SymbolKind::Enum));
        assert_eq!(find(&syms, "T").descriptor.kind, Some(SymbolKind::Trait));
        // トレイト内のメソッドシグネチャは `T::req`（型スコープ内＝Method）。
        assert_eq!(
            find(&syms, "T::req").descriptor.kind,
            Some(SymbolKind::Method)
        );
        assert_eq!(find(&syms, "C").descriptor.kind, Some(SymbolKind::Const));
        assert_eq!(find(&syms, "S").descriptor.kind, Some(SymbolKind::Static));
        assert_eq!(find(&syms, "Alias").descriptor.kind, Some(SymbolKind::Type));
        assert_eq!(find(&syms, "mac").descriptor.kind, Some(SymbolKind::Macro));
    }

    /// 観点: ジェネリックや経路付きの impl 型でも、基底の型名でスコープが付くこと
    /// （impl_type_name のジェネリック除去・最終セグメント抽出を担保）。
    #[test]
    fn rust_impl_type_name_strips_generics_and_paths() {
        let src = "struct Foo;\nimpl Foo<u8> { fn a() {} }\n";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        // `impl Foo<u8>` → 基底名 "Foo" → メソッドは "Foo::a"
        assert!(paths(&syms).contains(&"Foo::a".to_string()));
    }

    /// 観点: 関数本体の中の定義は拾わない（本体に降りない設計の確認）。
    #[test]
    fn rust_does_not_descend_into_function_bodies() {
        let src = "fn outer() { fn inner_local() {} }\n";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        let p = paths(&syms);
        assert!(p.contains(&"outer".to_string()));
        assert!(!p.contains(&"inner_local".to_string()));
    }

    /// 観点: range / name_range が埋まり、位置が妥当（行は 0 始まり）。
    /// node_range の動作確認。2 行目の関数は line==1。
    #[test]
    fn rust_ranges_are_populated() {
        let src = "struct Foo;\nfn free() {}\n";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        let free = find(&syms, "free");
        assert_eq!(free.range.start.line, 1); // 2 行目
                                              // name_range は range に含まれる（名前は定義の内側）。
        assert!(free.name_range.start.character >= free.range.start.character);
    }

    /// 観点: 構文エラーや空入力でもパニックせず、空でも安全に返ること。
    #[test]
    fn rust_handles_garbage_input() {
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), "fn (((");
        // 取れても取れなくてもよいが、パニックしないことが重要。
        let _ = syms.len();
    }

    /// 観点: struct の名前付きフィールドが `Type::field`（Field, 2段）、
    /// enum のバリアントが `Enum::Variant`（Variant, 2段）で拾えること。
    /// モジュール内なら更に前段が付く（`m::S::f`）。タプル構造体は名前無しなので拾わない。
    #[test]
    fn rust_extracts_fields_and_variants() {
        let src = "\
struct Config { timeout: u32, name: String }
enum State { Idle, Running }
struct Tuple(u8, u8);
mod m { struct Inner { v: u8 } }
";
        let syms = extract_rust_symbols(&RelPath("x.rs".into()), src);
        // フィールド
        assert_eq!(
            find(&syms, "Config::timeout").descriptor.kind,
            Some(SymbolKind::Field)
        );
        assert_eq!(
            find(&syms, "Config::name").descriptor.kind,
            Some(SymbolKind::Field)
        );
        // バリアント
        assert_eq!(
            find(&syms, "State::Idle").descriptor.kind,
            Some(SymbolKind::Variant)
        );
        assert_eq!(
            find(&syms, "State::Running").descriptor.kind,
            Some(SymbolKind::Variant)
        );
        // モジュール内のフィールドは 3 段。
        assert_eq!(find(&syms, "m::Inner::v").descriptor.path.0.len(), 3);
        // タプル構造体の要素は名前が無いので拾われない（Tuple 自体は Struct として出る）。
        let p = paths(&syms);
        assert!(p.contains(&"Tuple".to_string()));
        assert!(!p.iter().any(|q| q.starts_with("Tuple::")));
    }

    // ----- C: 名前 1 段（collect_c / declarator_name / emit_* を間接検証） -----

    /// 観点: 関数定義・グローバル変数・static 変数が拾えること。
    /// C の declarator（ポインタ・配列）を辿って名前を取る点も含む。
    #[test]
    fn c_extracts_functions_and_globals() {
        let src = "\
int g_count;
static unsigned char g_buf[10];
int *g_ptr;
void do_work(void) {}
";
        let syms = extract_c_symbols(&RelPath("x.c".into()), src);
        assert_eq!(
            find(&syms, "g_count").descriptor.kind,
            Some(SymbolKind::Variable)
        );
        // static → Static、配列宣言子を辿って "g_buf"
        assert_eq!(
            find(&syms, "g_buf").descriptor.kind,
            Some(SymbolKind::Static)
        );
        // ポインタ宣言子を辿って "g_ptr"
        assert_eq!(
            find(&syms, "g_ptr").descriptor.kind,
            Some(SymbolKind::Variable)
        );
        assert_eq!(
            find(&syms, "do_work").descriptor.kind,
            Some(SymbolKind::Function)
        );
    }

    /// 観点: 関数プロトタイプ（宣言のみ）は変数として拾わないこと。
    #[test]
    fn c_skips_function_prototypes() {
        let src = "void foo(void);\nint bar(int x);\n";
        let syms = extract_c_symbols(&RelPath("x.c".into()), src);
        // プロトタイプは Variable にも Function にもしない（定義側で扱う）。
        assert!(
            syms.is_empty(),
            "prototypes should not be extracted, got {:?}",
            paths(&syms)
        );
    }

    /// 観点: struct / enum 定義、typedef（単純・struct 付き）、#define が拾えること。
    #[test]
    fn c_extracts_types_and_macros() {
        let src = "\
struct Point { int x; int y; };
enum Color { RED, GREEN };
typedef unsigned char byte_t;
typedef struct LinFrame { int id; } LinFrame_t;
#define MAX 10
#define SQ(x) ((x)*(x))
";
        let syms = extract_c_symbols(&RelPath("x.c".into()), src);
        let p = paths(&syms);
        assert_eq!(
            find(&syms, "Point").descriptor.kind,
            Some(SymbolKind::Struct)
        );
        assert_eq!(find(&syms, "Color").descriptor.kind, Some(SymbolKind::Enum));
        assert_eq!(
            find(&syms, "byte_t").descriptor.kind,
            Some(SymbolKind::Type)
        );
        // typedef struct: 中の struct 名 LinFrame（Struct）と typedef 名 LinFrame_t（Type）の両方。
        assert!(p.contains(&"LinFrame".to_string()));
        assert_eq!(
            find(&syms, "LinFrame_t").descriptor.kind,
            Some(SymbolKind::Type)
        );
        assert_eq!(find(&syms, "MAX").descriptor.kind, Some(SymbolKind::Macro));
        assert_eq!(find(&syms, "SQ").descriptor.kind, Some(SymbolKind::Macro));
    }

    /// 観点: C のトップレベル記号（関数・型・変数）の修飾名は 1 段（D6）。
    /// （フィールドは `[Tag, field]` の 2 段になるが、それは別テストで確認。）
    #[test]
    fn c_paths_are_single_segment() {
        let src = "int foo(void) { return 0; }\n";
        let syms = extract_c_symbols(&RelPath("x.c".into()), src);
        assert_eq!(find(&syms, "foo").descriptor.path.0.len(), 1);
    }

    /// 観点: 構造体フィールドが `[タグ名, フィールド名]` の Field として拾えること。
    /// ポインタ宣言子（`char *label`）も declarator_name で辿れること。
    #[test]
    fn c_extracts_struct_fields() {
        let src = "struct Point { int x; int y; char *label; };\n";
        let syms = extract_c_symbols(&RelPath("x.c".into()), src);
        assert_eq!(
            find(&syms, "Point").descriptor.kind,
            Some(SymbolKind::Struct)
        );
        assert_eq!(
            find(&syms, "Point::x").descriptor.kind,
            Some(SymbolKind::Field)
        );
        assert_eq!(
            find(&syms, "Point::y").descriptor.kind,
            Some(SymbolKind::Field)
        );
        assert_eq!(
            find(&syms, "Point::label").descriptor.kind,
            Some(SymbolKind::Field)
        );
        // フィールドの修飾名は 2 段。
        assert_eq!(find(&syms, "Point::x").descriptor.path.0.len(), 2);
    }

    /// 観点: enum の列挙子が Const として拾えること。C ではグローバルなので 1 段
    /// （`Color::RED` ではなく `RED`）。値付き列挙子（`GREEN = 5`）も拾える。
    #[test]
    fn c_extracts_enum_constants() {
        let src = "enum Color { RED, GREEN = 5, BLUE };\n";
        let syms = extract_c_symbols(&RelPath("x.c".into()), src);
        assert_eq!(find(&syms, "Color").descriptor.kind, Some(SymbolKind::Enum));
        assert_eq!(find(&syms, "RED").descriptor.kind, Some(SymbolKind::Const));
        assert_eq!(
            find(&syms, "GREEN").descriptor.kind,
            Some(SymbolKind::Const)
        );
        assert_eq!(find(&syms, "BLUE").descriptor.kind, Some(SymbolKind::Const));
        assert_eq!(find(&syms, "RED").descriptor.path.0.len(), 1);
    }

    // ----- ディスパッチャ ext_of / extract_symbols -----

    /// 観点: 拡張子で言語が振り分けられること（.rs→Rust / .c,.h→C / その他→空）。
    #[test]
    fn dispatch_by_extension() {
        let rs = extract_symbols(&RelPath("a.rs".into()), "fn f() {}");
        assert_eq!(paths(&rs), vec!["f".to_string()]);

        let c = extract_symbols(&RelPath("a.c".into()), "void g(void){}");
        assert_eq!(paths(&c), vec!["g".to_string()]);

        let h = extract_symbols(&RelPath("a.h".into()), "#define K 1");
        assert_eq!(paths(&h), vec!["K".to_string()]);

        // 未対応拡張子は空。
        let txt = extract_symbols(&RelPath("a.txt".into()), "fn f() {}");
        assert!(txt.is_empty());
    }

    // ----- SymbolTable: intern / resolve / len / is_empty / iter -----

    /// 観点: 新規テーブルは空。intern で順番に ID が振られ、len が増える。
    #[test]
    fn table_intern_assigns_sequential_ids() {
        let mut table = SymbolTable::default();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);

        let syms = extract_rust_symbols(&RelPath("x.rs".into()), "fn a() {}\nfn b() {}");
        let id0 = table.intern(syms[0].clone());
        let id1 = table.intern(syms[1].clone());
        assert_eq!(id0, SymbolId(0));
        assert_eq!(id1, SymbolId(1));
        assert_eq!(table.len(), 2);
        assert!(!table.is_empty());
    }

    /// 観点: 一意なら Resolved（range 付き）、無ければ Dangling。
    #[test]
    fn table_resolves_and_dangles() {
        let mut table = SymbolTable::default();
        for e in extract_rust_symbols(&RelPath("x.rs".into()), "impl Foo { fn bar() {} }") {
            table.intern(e);
        }
        let hit = SymbolDescriptor {
            file: RelPath("x.rs".into()),
            path: SymbolPath(vec!["Foo".into(), "bar".into()]),
            kind: None, // 種別やヒントは解決に使わない＝None でも当たる
            disambiguator: None,
        };
        assert!(matches!(table.resolve(&hit), Resolution::Resolved { .. }));

        let miss = SymbolDescriptor {
            file: RelPath("x.rs".into()),
            path: SymbolPath(vec!["Nope".into()]),
            kind: None,
            disambiguator: None,
        };
        assert!(matches!(table.resolve(&miss), Resolution::Dangling));
    }

    /// 観点: 同一（ファイル, 修飾名）が複数あると Ambiguous（オーバーロード相当）。
    #[test]
    fn table_reports_ambiguous() {
        let mut table = SymbolTable::default();
        // 同じ記述子の実体を 2 件 intern して衝突を作る。
        let desc = SymbolDescriptor {
            file: RelPath("x.c".into()),
            path: SymbolPath(vec!["parse".into()]),
            kind: Some(SymbolKind::Function),
            disambiguator: None,
        };
        let zero = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        };
        table.intern(SymbolEntry {
            descriptor: desc.clone(),
            range: zero,
            name_range: zero,
        });
        table.intern(SymbolEntry {
            descriptor: desc.clone(),
            range: zero,
            name_range: zero,
        });
        match table.resolve(&desc) {
            Resolution::Ambiguous(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    /// 観点: iter() が (ID, 実体) を全件・正しい ID で返すこと。
    #[test]
    fn table_iter_yields_all_with_ids() {
        let mut table = SymbolTable::default();
        for e in extract_rust_symbols(&RelPath("x.rs".into()), "fn a() {}\nfn b() {}") {
            table.intern(e);
        }
        let collected: Vec<(SymbolId, String)> = table
            .iter()
            .map(|(id, e)| (id, e.descriptor.path.0.join("::")))
            .collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].0, SymbolId(0));
        assert_eq!(collected[1].0, SymbolId(1));
    }
}
