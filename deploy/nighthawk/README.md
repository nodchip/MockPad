# nighthawkへの初回デプロイ

公開先は `https://kishibe.dyndns.tv/mockpad/`。既存のHTTPSサーバーにlocationを追加し、Rustは`127.0.0.1:3001`だけで待ち受けます。nginxは子パスを除去せず転送し、利用者の`X-Real-IP`を上書きします。

## 配置ファイルの作成

Windows / PowerShellでの例です。

```powershell
$env:MOCKPAD_BASE_PATH='/mockpad'
npm.cmd run build
rustup target add x86_64-unknown-linux-musl
$env:RUSTFLAGS='-C linker=rust-lld'
cargo build --release --locked --target x86_64-unknown-linux-musl
Remove-Item Env:RUSTFLAGS
node deploy/nighthawk/package.mjs
```

`.cache/deploy/<release>.tar.gz`を`/home/nodchip/mockpad-deploy/`へ転送し、表示されたSHA-256を照合してから展開します。バイナリには実行権限を付けてください。マニフェストにはバイナリ・配信ファイル・設定・適用スクリプトのチェックサムと、コミット未反映の有無を記録します。

## 公開前検証

```sh
python3 /home/nodchip/mockpad-deploy/<release>/install.py --check
```

`preview.py`と`smoke.py`を同じ親ディレクトリへ転送すると、次のコマンドで実機検証できます。管理者権限は不要です。

```sh
python3 /home/nodchip/mockpad-deploy/smoke.py /home/nodchip/mockpad-deploy/<release>
```

検証用のユーザーサービスとnginxをループバックの3001・3189番で起動し、子パスのHTML・アセット・Cookie・WebSocket編集・再同期・終了権限を確認して停止します。既存のnginxサービスには触れません。

## 最終適用（利用者が実行）

```sh
ssh -t nighthawk "sudo python3 /home/nodchip/mockpad-deploy/<release>/install.py"
```

`<release>`を検証済みのリリース名に置き換えます。sudoの認証が必要です。専用ユーザー`mockpad`、`/opt/mockpad/releases/<release>`、systemdサービス、nginx設定を作成します。nginxの既存設定のSHA-256が変わっていた場合や既存のMockPad配置がある場合は停止します。このスクリプトは初回配置用です。

既存サイト設定は`/var/backups/mockpad-<release>/nginx-default`に保存します。設定検査、サービス起動確認、HTTPS配信確認に失敗した場合は元のnginx設定を戻し、新しいサービスを停止・無効化します。適用後の状態は次のコマンドで確認できます。

nginxの再読み込みはシグナル送信後に設定が反映されるため、HTTPS確認は最大約30秒再試行します。応答HTMLのSHA-256が配置ファイルと一致したときだけ成功とします。失敗した配置のリリースとバックアップは保持されるため、再試行には新しいリリース名を使います。

```sh
ssh nighthawk "systemctl is-active mockpad nginx"
```

部屋と編集内容はメモリー上にだけ存在します。プロセスの停止・再起動で失われます。
