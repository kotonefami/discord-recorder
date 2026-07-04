<div align="center">
  <h1>discord-recorder</h1>
  Supported with 💛 by Kisaragi Project
</div>

## 概要

Discord ボイスチャンネルの音声を録音する Bot です。

指定したボイスチャンネルにユーザーが参加すると自動で録音を開始し、全員が退出すると自動で終了します。
ユーザーごとに個別の Ogg Opus ファイル (`output/<日時>/<ユーザー名>.opus`) が出力されます。

無音フレームを埋め込むことで、パケットが送信されなかった無音時間帯も含めて、ユーザーの発言タイミングを正確に記録します。

## 使い方

### 環境変数の設定

`.env.example` を `.env` にコピーし、以下の値を設定します。

```env
DISCORD_TOKEN=your_token_here
DISCORD_CHANNEL_ID=123456789012345678
```

コマンドライン引数で渡すことも可能です。

```sh
cargo run -- <DISCORD_TOKEN> <DISCORD_CHANNEL_ID>
```

### 実行

```sh
cargo run --release
```

## 動作の仕組み

- Bot は録音対象チャンネルに入り、ミュート状態で傍受します。
- ユーザーが話すたびに `SpeakingStateUpdate` / `VoiceTick` イベントを受け取り、Opus エンコードされた音声をファイルに書き込みます。
- 録音開始時に既にチャンネルにいたユーザーには無音フレームが埋め込まれ、タイミングのズレを防ぎます。

## 依存関係

- [serenity](https://github.com/serenity-rs/serenity) - Discord API クライアント
- [songbird](https://github.com/serenity-rs/songbird) - 音声接続ライブラリ
- [audiopus](https://github.com/haata/audiopus) - Opus エンコーダー
- [ogg](https://github.com/nickel-org/ogg.rs) - Ogg コンテナライター

## ライセンス

[Unlicense](LICENSE)
