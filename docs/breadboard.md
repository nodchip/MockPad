---
shaping: true
---

# MockPad — Breadboard

[Shape A](shaping.md)の実装に同期した画面・操作・データフロー（2026-10-24）。
実装は`src/main.rs`と`web/main.ts`。関数と匿名コールバックの実際の責務を記録する。
部屋作成から終了までを対象にし、Discordの通話・講評は境界の外に置く。

## Places

| # | Place | 内容 |
|---|---|---|
| P1 | 部屋作成画面 | 面接官の表示名を入力して部屋を作る |
| P2 | 入室画面 | 参加URLから部屋を確認し、表示名を入力する |
| P3 | 共同編集画面 | コード、共有設定、参加者、接続状態、参加URLを扱う |
| P4 | 終了確認ダイアログ | P3の操作をブロックし、終了またはキャンセルを選ぶ |
| P5 | 部屋を利用できない画面 | 終了済み・期限切れ・存在しない部屋を表示する |
| P6 | Rustサーバー | 部屋レジストリー、同期、管理権限、接続と期限を処理するバックエンド境界 |

## UI affordances

| # | Place | Component | Affordance | Control | Wires Out | Returns To |
|---|---|---|---|---|---|---|
| U1 | P1 | 作成フォーム | 面接官の表示名 | type | → S3 | — |
| U2 | P1 | 作成フォーム | 部屋を作成 | submit | → N1 | — |
| U3 | P1/P2/P3/P4 | メッセージ | 入力・通信・操作エラー | render | — | — |
| U4 | P2 | 入室フォーム | 参加者の表示名 | type | → S3 | — |
| U5 | P2 | 入室フォーム | 入室する | submit | → N3 | — |
| U6 | P3 | Monaco | コード入力・表示・選択・Undo/Redo | edit/key/render | → N6, N9, N18 | — |
| U7 | P3 | 接続表示 | 接続中・同期中・接続済み・再接続中 | render | — | — |
| U8 | P3 | 参加者表示 | 名前・色・カーソル・選択範囲 | render | — | — |
| U9 | P3 | 言語選択 | 言語の指定と現在の言語 | change/render | → N7 | — |
| U10 | P3 | インデント設定 | タブサイズの指定と現在のサイズ | change/render | → N7 | — |
| U11 | P3 | 招待 | 参加URLをコピー | click | → N11 | — |
| U12 | P3 | 招待 | コピー成功・失敗 | render | — | — |
| U13 | P3 | 管理 | 部屋を終了（面接官だけ表示） | click | → P4 | — |
| U14 | P4 | 終了確認 | キャンセル | click | → P3 | — |
| U15 | P4 | 終了確認 | 終了する | click | → N13 | — |
| U16 | P5 | 終了表示 | 終了理由と新しい部屋作成へのリンク | render/click | → P1 | — |
| U17 | P3 | 履歴 | 元に戻すボタン | click/render | → N18 | — |
| U18 | P3 | 履歴 | やり直すボタン | click/render | → N18 | — |

## Code affordances

| # | Place | Component | Affordance | Control | Wires Out | Returns To |
|---|---|---|---|---|---|---|
| N1 | P1 | ブラウザー | `landing()`の作成submit分岐: 表示名の検証、作成APIの呼び出し、history更新、RoomEditor生成 | submit | → N2, S3, N4, P3 | → U3 |
| N2 | P6 | 作成API | `create_room()` / `Room::new()`: ID・シークレット・初期文書を作り、Cookieを設定 | POST /api/rooms | → N24, S1, S4 | → N1 |
| N3 | P2 | ブラウザー | `landing()`の参加分岐: 初期の部屋確認、表示名の検証、RoomEditor生成 | route/submit | → N21, S3, N4, N17, P3 | → U3 |
| N4 | P3 | 接続 | `RoomEditor.connect()`: メタデータ取得、管理ボタンの表示、WebSocket接続 | call/retry | → N21, N22, S3, N8, N9, N12, N17, N27 | → U13 |
| N5 | P6 | 文書同期 | `handle_message()`のSync分岐: state vectorへ応答し、受信更新をYrsへ適用して配信 | receive | → S1, S5, N22 | → N22 |
| N6 | P3 | 編集 | `doc.on('update')` / `RoomEditor.send()`: remote以外の更新を送信。切断中の文書はタブ内に保持 | observe | → S2, N22 | — |
| N7 | P3 | 設定 | language/tab-sizeの`onchange`: 接続済みなら共有Y.Mapを変更 | change | → S2, N6, N26 | — |
| N8 | P3 | 受信 | `socket.onmessage`: sync/awarenessをdecodeしremote originで適用。応答送信と同期完了を処理 | receive | → S2, N22, N26, N27, N19 | → U6 |
| N9 | P3 | Awareness | MonacoBindingの選択通知と`awareness.on('update')`: 自分のpresenceをencodeして送信 | cursor/change/open | → S2, N22 | — |
| N10 | P6 | Awareness | `handle_message()`のAwareness分岐: 接続ごとのclient IDを確認し適用・配信 | receive | → S5, N22 | — |
| N11 | P3 | 招待 | inviteの`onclick`: 参加URLを`navigator.clipboard.writeText()`へ渡して結果を表示 | click | → S6, S3 | → U12 |
| N12 | P3 | 再接続 | `RoomEditor.scheduleRetry()`: 表示・編集可否を変更し、相手のpresenceを除去して再接続を予約 | close/error/timer | → N27, S2, S3, N19, N4 | — |
| N13 | P4 | 管理 | confirm-closeの`onclick`: 終了APIを呼び、成功または既に不在なら終了表示 | click | → N14, N17, S3 | → U3 |
| N14 | P6 | 管理 | `close_room()`: OriginとCookieを検証し、ロック下で部屋を閉じてレジストリーから除去 | POST /api/rooms/:id/close | → N24, N16, S1 | → N13 |
| N15 | P6 | 切断 | `Room::disconnect()`: 接続とそのpresenceを除去し、最後の退出で不在開始時刻を記録 | call | → S1, S5, N22 | → N22 |
| N16 | P6 | 終了通知 | `Room::close()`: closedを設定し、全接続へ終了フレームを送り接続一覧を空にする | call | → S1, S5, N22 | → N14, N25, N28 |
| N17 | P2/P3/P4 | 終了処理 | `unavailable()` / `RoomEditor.dispose()`: 再試行・接続・モデルを破棄し利用不可画面を表示 | closed/404 | → S2, S3, N22, P5 | → U16 |
| N18 | P3 | Undo | モデルの`undo/redo`とaddCommand/addAction: 自分のMonacoBindingだけを追跡するY.UndoManagerを操作 | keyboard/button/menu | → S2, N6, N27 | → U6 |
| N19 | P3 | 参加者表示 | `RoomEditor.renderPresence()`とMonacoBinding: 参加者名・配色・相対選択位置からラベルと装飾を描画 | awareness update | — | → U8 |
| N20 | P6 | 起動 | `main()` / `router()`: 空のレジストリーで起動し、定期清掃タスク・API・静的配信を登録 | startup | → S1, N23, N28 | — |
| N21 | P6 | 部屋確認 | `room_metadata()`: 部屋と期限を確認し、Cookieに基づくisHostを返す。入室フォーム表示だけでは接続登録しない | GET /api/rooms/:id | → N25, N24 | → N3, N4 |
| N22 | P6 | WebSocket | `upgrade()` / `connect()`: 入室登録、初期同期、送受信、キューからの配信、切断時の後片付け | upgrade/receive/send | → N24, N25, S1, S5, N5, N10, N15, N29 | → N4, N8 |
| N23 | P3/P6 | 設定 | `ROOM_TTL`、環境変数、palette、Monacoの生成オプション、Viteのalias/転送設定 | config | — | → N2, N4, N12, N19, N20, N22, N26, N28, N29 |
| N24 | P6 | 権限 | `App::check_origin()` / `is_host()`: Originと管理Cookieを検証 | call | — | → N2, N14, N21, N22 |
| N25 | P6 | 部屋取得 | `App::lookup()`: レジストリーから取得し、期限切れ・終了済みなら閉じて除去 | call | → S1, N16 | → N21, N22 |
| N26 | P3 | 設定反映 | `RoomEditor.applySettings()`: 共有Y.Mapを検証し、Monacoの言語・インデントとselectを更新 | observe/call | → S2 | → U6, U9, U10 |
| N27 | P3 | 接続・履歴表示 | `setStatus()` / `updateUndo()`: 接続状態、編集可否、共有設定とUndoボタンの有効状態を反映 | call/undo event | → S3 | → U6, U7, U9, U10, U17, U18 |
| N28 | P6 | 清掃 | `App::clean()`: 60秒間隔で不在期限を確認し、部屋を閉じてレジストリーから除去 | timer | → S1, N16 | — |
| N29 | P6 | 接続監視 | `connect()`のheartbeat分岐: 15秒ごとのPing、45秒超の無応答でループを抜ける | heartbeat | → S5, N22, N15 | — |

## Data stores

| # | Place | Store | 内容・書き込み元 | Returns To |
|---|---|---|---|---|
| S1 | P6 | `App.rooms` / `Room`（メモリーのみ） | 部屋ID→Yrs文書、管理シークレット検証情報、不在開始時刻、終了状態。N2/N5/N14/N15/N16/N20/N22/N25/N28が操作 | → N2, N5, N14, N15, N16, N21, N22, N25, N28 |
| S2 | P3 | タブ内の編集状態 | Y.Doc/Y.Text/Y.Map、Monacoモデル、UndoManager、Awareness、同期状態。N4/N6/N7/N8/N9/N17/N18/N26が操作 | → N6, N8, N9, N18, N19, N26, U6, U8, U9, U10 |
| S3 | P1/P2/P3/P4/P5 | DOMとRoomEditorの画面状態 | 入力名、部屋ID、管理権限、接続状態、エラー、コピー結果、終了理由。U1/U4/N1/N3/N4/N11/N12/N13/N17/N27が操作 | → N1, N3, N4, N9, N11, N12, N13, N27, U3, U7, U12, U13, U16, U17, U18 |
| S4 | ブラウザー/P6 | 管理Cookie | N2がSet-Cookieで設定。HttpOnly/SameSiteで、部屋単位のAPIパスに限定。本番HTTPSではSecure。コードは保存しない | → N21, N24 |
| S5 | P6 | 接続セッション | `Room.peers`、Awarenessのpresence、`connect()`内のキュー・heartbeat状態。N5/N10/N15/N16/N22/N29が操作 | → N5, N10, N15, N16, N22, N29 |
| S6 | OS | クリップボード | N11が参加URLを書き込む外部副作用。Discordへの貼り付けは利用者が行う | → 外部のDiscord |

## データ源の確認

- U1/U4は利用者入力、U2/U5/U11/U14/U15は固定ラベル、U3/U7/U12/U13/U16はS3に基づく。
- U6/U8/U9/U10はS2をN8/N19/N26経由で表示する。U13の表示可否は、名前ではなくN4がN21から取得した管理権限に基づく。U17/U18の有効状態はN27が供給する。
- 言語・タブサイズの選択肢は初期設定、現在値はY.Map。面接官と受験者で同じ値を表示する。
- U7はWebSocketが開いた時点だけでは「接続済み」にしない。差分同期完了を待つ。
- N12は一時的な通信障害のみ再試行する。N21/N25が部屋不在・期限切れを返した場合はN17へ移る。
- S4のCookieはメタデータ取得・終了・WebSocketの各APIへ送れる共通の部屋APIパスに置く。サーバーはOriginも確認する。

## 主要な操作経路

1. **作成と共有**: U1→S3、U2→N1→N2→S1/S4→P3/N4。N4→N21→N25で確認し、N22→N8で初期同期、N27→U7。U11→N11→S6、結果→U12。Discordへの貼り付けは利用者が行う。
2. **入室**: 参加URL→P2/N3→N21→N25で存在確認。U4→S3、U5→N3→P3/N4→N22。文書→N8→S2→U6。存在しなければN17→P5。
3. **共同編集**: U6→N6→N22→N5→S1→N22→相手のN8→U6。U9/U10→N7→S2→N26でローカル設定を反映し、N6から同じ経路で共有する。
4. **指し示す**: U6の選択変更→N9→N22→N10→S5→N22→相手のN8→N19→U8。本文変更時の装飾再計算はMonacoBindingが行う。
5. **自分だけUndo**: U6のキーボード・メニューまたはU17/U18→N18→S2→N6→N22→N5→相手のN8→U6。設定と相手の変更は追跡しない。
6. **再接続**: 切断→N12→N27→U7/U6（編集不可）。N4→N21→N25→N22→差分同期→N8→N27で編集を再開。部屋不在ならN17→P5。
7. **終了**: U13→P4。U14ならP3。U15→N13→N14→N24（権限）→N16（終了通知）→S1除去。N22から両者のN17→P5/U16。権限・通信失敗ならP4/U3に留まる。
8. **不在・期限切れ**: N22の切断→N15→S1に不在時刻。期限内のN22入室で解除。24時間以上→N28またはN25→N16→S1除去。旧URLはN17→P5。
9. **再起動**: N20で空のS1から開始。古いURLに対してN25は作成せず404を返し、N4/N3→N17へ移る。タブ内の未送信文書からも部屋を復活させない。

## 部屋の状態遷移

```text
作成 → 全員不在（作成時刻から起算）
全員不在 → 入室（期限内） → 接続中
接続中 → 最後の接続が切断 → 全員不在（この時刻から起算）
接続中／全員不在 → 面接官の終了操作 → 削除
全員不在 → 不在24時間経過 → 削除
すべての状態 → サーバー再起動 → 消失
削除／消失 → 古いURLからアクセス → 利用不可
```

入室・更新・切断・終了・期限処理は部屋単位で直列化する。
期限確認と入室登録の間に競合を作らず、削除した部屋への古いハンドルからの更新を拒否する。

## Reflectionの結果と検証状態

SEEでAPI・WebSocket・文書同期を別の実装経路として確認し、旧N5に混在していた責務をN5/N21/N22/N25へ分離した。旧N15に混在した退出・清掃・heartbeatもN15/N28/N29として実装どおりに記録した。共有設定のローカル反映（N26）、接続とボタン有効状態（N27）、実際のUndoボタン（U17/U18）も追加した。

REFLECTでは、部屋作成から終了までの9経路を追跡し、メタデータ取得だけで入室したように見える旧配線と、再接続後の編集可否の隠れた処理を解消した。上記の境界分離は表の修正であり、不要なサーバー抽象化は追加していない。

本体のAPI・WebSocketを通す14項目のE2Eと、期限・権限・削除を扱うRustの5テストが合格。見た目も確認済み。macOS実機や実インターネット上の端末間通信などの未検証範囲は[スライス](slices.md)に記録する。
