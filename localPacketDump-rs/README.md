# Packet Monitor - eth2 Interface Traffic Monitor

Rust で実装された、eth2 ネットワークインターフェースの通信を監視するプログラムです。

## 機能

- **eth2 インターフェースの監視**: eth2 インターフェースを通じた全ての通信をキャプチャ
- **通信先 IP の追跡**: 通信先の IP アドレスを記録
- **通信量の計測**: 1 秒単位で通信量を計測し続ける
- **Prometheus メトリクス出力**: localhost:59122 でメトリクスを公開

## 出力形式

Prometheus 形式でメトリクスを提供します：

```
# Prometheus メトリクス
packet_total_bytes             # 合計転送バイト数
packet_total_count             # 合計パケット数
packet_unique_destinations     # ユニークな通信先 IP 数
traffic_by_destination_bytes   # 通信先 IP ごとの転送バイト数
```

## インストール

```bash
cargo build --release
```

## 実行

```bash
cargo run --release
```

## Prometheus 設定

`prometheus.yaml` に以下を追加：

```yaml
- job_name: "localpacketdump"
  scrape_interval: 1s
  static_configs:
    - targets: ["localhost:59122"]
```

## メトリクス取得

```bash
curl http://localhost:59122/metrics
```

## 出力例

```
# HELP packet_total_bytes Total bytes transferred
# TYPE packet_total_bytes counter
packet_total_bytes 524288

# HELP packet_total_count Total packets transferred
# TYPE packet_total_count counter
packet_total_count 512

# HELP packet_unique_destinations Number of unique destination IPs
# TYPE packet_unique_destinations gauge
packet_unique_destinations 24

# HELP traffic_by_destination_bytes Traffic in bytes per destination
# TYPE traffic_by_destination_bytes gauge
traffic_by_destination_bytes{destination="192.168.1.100"} 102400
traffic_by_destination_bytes{destination="10.0.0.50"} 204800
traffic_by_destination_bytes{destination="172.16.0.200"} 217088
```

## 注意事項

- このプログラムは `root` 権限が必要です（パケットキャプチャのため）
- eth2 インターフェースが存在する環境で実行してください
- インターフェースが見つからない場合は 5 秒ごとに再試行します

## ビルドと実行例

```bash
# ビルド
cargo build --release

# 実行（root 権限が必要）
sudo ./target/release/packet_monitor

# メトリクス確認
curl http://localhost:59122/metrics
```
