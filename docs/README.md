# hype-trigger-twap 日本語ドキュメント

Hyperliquid 無期限先物向けの「トリガー付き TWAP」を実行する単一 Rust バイナリ (`hype-twap`) の
日本語資料です。サーバー・WebSocket・Python・DB は一切使いません。

指定した価格または経過時間で発火し、目標数量を等間隔の IOC (テイカー) スライスに分割して
執行します。スライスが約定不足のときは次スライスで自動的に取り返します (キャッチアップ方式)。

**Read-only (ドライラン) が既定値です。** `--read-only false` を明示しない限り注文は一切送信されません。

## 目次

| ドキュメント | 内容 |
|---|---|
| [使い方 (USAGE.md)](USAGE.md) | インストール、コマンド例、全フラグ、環境変数、終了コード |
| [仕組み (DESIGN.md)](DESIGN.md) | トリガー、サイジングと丸め、スライスループ、エラー処理の設計 |
| [運用ガイド (OPERATIONS.md)](OPERATIONS.md) | 事前準備、初回実行手順、監視、トラブルシューティング、既知の制約 |
| [開発 (DEVELOPMENT.md)](DEVELOPMENT.md) | ビルド・テスト、モジュール構成、署名コアの扱い、貢献時の注意 |

英語版の概要は最上位の [README.md](../README.md) を参照してください。

## 最短の使い方

```bash
# ビルド
cargo build --release

# ドライラン (既定) — 実際には発注せず、板から計算した想定注文を表示
./target/release/hype-twap --symbol HYPE --side long --usd 1500 --duration 30m

# 本番実行
export HL_AGENT_PK=0x<64桁の16進数>
./target/release/hype-twap --symbol HYPE --side long --usd 1500 --duration 30m --read-only false
```

## 安全設計の要点

このツールは実資金を扱うため、以下を既定の振る舞いとしています。

- **既定はドライラン** — `--read-only false` を明示するまで署名も送信も行いません
- **不明な銘柄は起動時に停止** — 発注前に `/info meta` と照合します
- **取引所の拒否は即時停止** — 証拠金不足・最低数量割れは再試行せず全体を止めます
- **秘密鍵は環境変数のみ** — コマンドライン引数では受け取らないため、シェル履歴や `ps` に残りません
- **曖昧な送信は cloid で照合** — 応答が失われた注文は再送前に実際の状態を問い合わせ、二重約定を防ぎます

## ライセンス

Apache-2.0。詳細は [LICENSE](../LICENSE) を参照してください。
