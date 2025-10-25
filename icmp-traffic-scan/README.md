# ICMP RTT Monitor

Prometheus からネットワークメトリクスを取得し、各リモート IP に対して ICMP パケットを送信して RTT（往復時間）を測定し、Prometheus exporter として公開するツール。

## 機能

1. **Prometheus メトリクス取得**: localhost:9090 から `localpacketdump` ジョブのメトリクスを定期的に取得
2. **リモート IP 抽出**: 取得したメトリクスから `remote_ip` と `interface` を抽出
3. **データ量フィルタリング**: 100 バイト以下のデータは測定対象外
4. **ICMP Ping**: 各リモート IP に対して ICMP ping を実行し、RTT を測定
5. **Prometheus Exporter**: localhost:59123 でメトリクスを公開

## ビルド

```bash
cargo build --release
```

## 実行

```bash
./target/release/icmp_monitor
```

## Prometheus 設定

以下を `prometheus.yml` に追加してください：

```yaml
- job_name: "rtticmpdump"
  scrape_interval: 1s
  static_configs:
    - targets: ["localhost:59123"]
```

## メトリクス

出力されるメトリクス：

- `rtt_icmp_dump{remote_ip="<IP>", interface="<IFACE>", data_type="upload"}` - アップロード方向の RTT（ミリ秒）
- `rtt_icmp_dump{remote_ip="<IP>", interface="<IFACE>", data_type="download"}` - ダウンロード方向の RTT（ミリ秒）

例：

```
rtt_icmp_dump{remote_ip="1.0.0.1", interface="eth0", data_type="download"} 42.5
rtt_icmp_dump{remote_ip="1.0.0.1", interface="eth1", data_type="upload"} 43.2
```

## 実装の特徴

- **並列実行**: 複数の IP に対する ICMP ping を並列実行し、測定効率を向上
- **非同期処理**: Tokio を使用した完全な非同期実装
- **ロギング**: tracing を使用した詳細なログ出力

## 要件

- Linux または macOS（ICMP ping コマンド必須）
- Prometheus 9090 ポートで実行中
- 管理者権限（ICMP ping 実行用）
# icmp-traffic-scan
