ローカルの検査を CI と同じ内容で一通り走らせ、結果を報告してください。失敗があれば原因を特定して修正し、再実行してグリーンにしてから終えること。

実行する検査:

1. `cargo fmt --all --check`（整形ずれ）
2. `cargo clippy --all-targets --all-features -- -D warnings`（lint）
3. `cargo test --all`（テスト）
4. `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`（Pages と同じ仕様ビルド／壊れた doc リンク検出）
5. `editor/` 配下に変更がある場合のみ `cd editor && npm run build`（型チェック＋bundle）

各検査の合否を簡潔に一覧で示し、最後に「コミットしてよい状態か」を一言で結論づけてください。
