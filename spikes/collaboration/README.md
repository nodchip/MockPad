# 共同編集構成の技術スパイク

実施日: 2026-10-24（日本時間）。MockPad本体ではなく、採用する構成の互換性を確かめる検証プログラム。

## 構成

- ブラウザー: Monaco Editor **0.57.0**、Yjs **13.6.33**、y-monaco **0.1.6**、y-protocols **1.0.7**。
- Rust: Yrs **0.28.0**。Yjs 13に合わせて **small-client** を有効化。
- 検証実行: Node.js 24.22.1、Vite 8.0.3、Playwright Core 1.64.1、Microsoft Edge 155.0.4283.45（headless）。
- 同一ページの二つの変数ではなく、独立したブラウザーコンテキスト2つに、それぞれMonacoとY.Docを生成。
- ページ→ローカルHTTPブリッジ→Rust子プロセスのstdin→Yrs文書→差分応答という経路。
- Node/Rustの依存関係はpackage-lock.json/Cargo.lockで固定。

## 結果

`npm test`は終了コード0。次の7項目に合格した。

| 検証 | 確認したこと |
|---|---|
| Yjs–Yrs round trip | 日本語と絵文字を含む本文がRustを往復して一致する |
| Concurrent edits | 未同期の別々の編集が、同じYrs文書を経由して両者に残り、最終内容が一致する |
| Local Undo/Redo | Ctrl+Zで自分の変更だけが消えて相手の変更は残り、Ctrl+Yで自分の変更だけが戻る |
| Shared settings | 言語とタブサイズをY.Mapで変更すると、Rust経由で相手のMonaco設定にも反映される |
| Reconnect data merge | Playwrightで片方の通信をオフラインにし、両側の変更を通信復旧後の明示同期で統合できる |
| Presence binding | 名前・色・相対選択位置を別コンテキストへ渡すと、相手側の状態とMonacoの選択デコレーションが生成される |
| Indentation | JavaScriptの開き波括弧の後のEnterで4スペースが入り、Tabでもスペースで次の位置に進む |

機械可読な最新結果は実行時に`results.json`へ書き出す（生成物のためバージョン管理対象外）。

## 見つかった互換性上の注意

Monaco 0.56以降で公開ESMパスが変更され、y-monaco 0.1.6は旧パス
`monaco-editor/esm/vs/editor/editor.api.js`を参照していた。
調整前の実行はimport解決で失敗した。Viteのaliasで公開パス`monaco-editor/editor`へ接続すると、7項目すべて合格した。
本体でもこのaliasを適用する。node_modulesの直接編集は不要。

Yrs 0.28.0の既定クライアントID形式と、Yjs 13の形式を合わせるため、Cargo.tomlの`small-client`指定を維持する。

## 確認していないこと

- 本番用WebSocket通信、標準同期メッセージのフレーミング、自動再接続、heartbeat。
- Rust経由のAwareness転送。今回はブラウザー間の検証用転送で状態とデコレーション生成を確認した。
- 参加者ラベル・色・カーソルの見た目。デコレーションは生成されるが、CSSと名前ラベルは本体で実装する。
- 部屋作成・管理権限・終了・24時間の期限切れ・サーバー再起動時の画面。
- 全言語のハイライトとインデント、macOS・メニュー経由などすべてのUndo操作。
- サービスの公開、負荷、異なる端末間の実通信。

これらは[実装スライス](../../docs/slices.md)の受け入れ条件として残す。

## 再実行（Windows / PowerShell）

ワークスペース直下から:

```powershell
$env:CARGO_HOME = Join-Path (Get-Location) '.cache/cargo'
cargo build --manifest-path spikes/collaboration/rust/Cargo.toml --locked
Push-Location spikes/collaboration
npm.cmd ci --cache .cache/npm --no-audit --no-fund
npm.cmd test
Pop-Location
```

Rust、Node.js、Microsoft Edgeのインストールが必要。初回の依存取得にはネットワーク接続が必要。
テストが起動したローカルHTTPサーバー、ブラウザー、Rust子プロセスは終了時に停止する。

## 一次資料

- [Monacoの公開モジュール変更](https://github.com/microsoft/monaco-editor/blob/main/CHANGELOG.md)
- [y-monacoの連携実装](https://github.com/yjs/y-monaco/blob/master/src/y-monaco.js)
- [Yjs UndoManagerとtrackedOrigins](https://docs.yjs.dev/api/undo-manager)
- [Yjs Awareness](https://docs.yjs.dev/getting-started/adding-awareness)
- [Yrsの機能と互換性設定](https://docs.rs/yrs/latest/yrs/)
