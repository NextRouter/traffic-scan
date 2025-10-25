# Throughput Dump

Prometheus からメトリクスを取得し、TCP ウィンドウサイズと RTT に基づいてスループットを計算するプログラムです。

## 概要

このプログラムは以下の処理を行います：

1. Prometheus から以下のメトリクスを取得：

   - `rtt_icmp_dump` - RTT（Round Trip Time）
   - `download_bytes` - ダウンロードバイト数
   - `upload_bytes` - アップロードバイト数

2. インターフェースとリモート IP ごとにスループットを計算：

   ```
   throughput = (download_bytes + upload_bytes) / rtt_icmp_dump
   ```

3. 計算結果を `throughputdump` メトリクスとしてポート 59124 で公開

## 必要要件

- Rust (2021 edition 以降)
- Prometheus（ローカルホストで稼働）

## セットアップ

### 1. ビルド

```bash
cargo build --release
```

### 2. 実行

```bash
# デフォルト（Prometheus: http://localhost:9090）
cargo run --release

# カスタムPrometheus URL
PROMETHEUS_URL=http://your-prometheus:9090 cargo run --release
```

### 3. ログレベル設定

```bash
# デバッグログを有効化
RUST_LOG=debug cargo run --release

# infoレベルのログ
RUST_LOG=info cargo run --release
```

## Prometheus の設定

`prometheus.yaml` に以下の設定を追加：

```yaml
scrape_configs:
  # スループットダンプ監視
  - job_name: "throughput-dump"
    scrape_interval: 1s
    static_configs:
      - targets: ["localhost:59124"]
```

## メトリクス

### 出力メトリクス

- **メトリクス名**: `throughputdump`
- **ラベル**:
  - `interface`: ネットワークインターフェース名（例: eth0）
  - `remote_ip`: リモート IP アドレス（例: 104.17.107.111）
  - `job`: "throughputdump"

### メトリクスの確認

```bash
curl http://localhost:59124/metrics
```

出力例：

```
# HELP throughputdump Calculated throughput based on bytes and RTT
# TYPE throughputdump gauge
throughputdump{interface="eth0",job="throughputdump",remote_ip="104.17.107.111"} 12345.67
```

## 仕様

- 1 秒間隔で Prometheus からメトリクスを取得
- インターフェースとリモート IP の組み合わせごとに計算
- RTT が 0 以下の場合はスキップ
- 計算結果は即座に Prometheus メトリクスとして公開

## トラブルシューティング

### Prometheus に接続できない

環境変数 `PROMETHEUS_URL` を確認してください：

```bash
PROMETHEUS_URL=http://localhost:9090 cargo run --release
```

### メトリクスが表示されない

1. Prometheus で必要なメトリクスが存在するか確認
2. ログを確認してエラーがないかチェック
3. インターフェース名とリモート IP が正しいか確認

## ライセンス

MIT
# throughput-dump
