//! # Developer Brain CLI
//!
//! ヘッドレス（GUI なし）でコアを叩くための入口です。3 つの用途があります:
//!
//! 1. **開発中の検証** … エディタを開かずに索引結果を確認する。
//! 2. **CI** … 「索引が壊れていないか」を自動テストで回す。
//! 3. **AI 連携の布石** … エージェントが `dbrain index .` を実行して
//!    グラフ JSON を読む、という使い方を見越している。だから出力は
//!    人間向けの飾りではなく **機械可読な JSON** を標準出力に出す。
//!
//! M1 ユニット1 で Markdown 側の索引（`dbrain-core::index`）に接続済み。
//! コードシンボルの解決（tree-sitter）は次のユニットで加わる。

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    // 実行時引数を集める。args[0] は実行ファイル名なので、args[1] 以降を見る。
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        // `dbrain index <dir>` : 指定ディレクトリを索引してグラフ JSON を出す。
        Some("index") => {
            let dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")); // 省略時はカレント。

            let idx = dbrain_core::index::index_workspace(&dir);

            // 人間向けの進捗は stderr、機械可読データ(JSON)は stdout に分ける。
            eprintln!(
                "[dbrain] {} nodes, {} edges (コード参照: 解決 {} / 未解決 {})",
                idx.graph.node_count(),
                idx.graph.edge_count(),
                idx.graph.code_refs_resolved,
                idx.graph.code_refs_unresolved
            );
            println!("{}", idx.graph.to_json_string());
            ExitCode::SUCCESS
        }
        // 使い方が分からない入力には usage を出して、非ゼロ終了する
        // （CI が「失敗」と判定できるよう、終了コードを 0 以外にする）。
        _ => {
            eprintln!("usage: dbrain index <dir>");
            ExitCode::from(2)
        }
    }
}
