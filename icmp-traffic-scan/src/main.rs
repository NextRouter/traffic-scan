use anyhow::Result;
use prometheus::{Encoder, GaugeVec, Registry, TextEncoder};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::task;
use tokio::time::sleep;
use tracing::{error, info};

#[derive(Debug, Clone)]
struct RemoteIpMetric {
    ip: String,
    interface: String,
    data_type: String, // "download" or "upload"
    bytes: u64,
}

struct MetricsCollector {
    rtt_gauge: GaugeVec,
    registry: Registry,
}

impl MetricsCollector {
    fn new() -> Result<Self> {
        let registry = Registry::new();

        let rtt_gauge = GaugeVec::new(
            prometheus::Opts::new(
                "rtt_icmp_dump",
                "RTT measured via ICMP ping in milliseconds",
            ),
            &["remote_ip", "interface", "data_type"],
        )?;

        registry.register(Box::new(rtt_gauge.clone()))?;

        Ok(MetricsCollector {
            rtt_gauge,
            registry,
        })
    }

    fn set_rtt(&self, remote_ip: &str, interface: &str, data_type: &str, rtt_ms: f64) {
        self.rtt_gauge
            .with_label_values(&[remote_ip, interface, data_type])
            .set(rtt_ms);
    }

    fn gather_metrics(&self) -> Result<String> {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = vec![];
        encoder.encode(&metric_families, &mut buffer)?;
        Ok(String::from_utf8(buffer)?)
    }
}

async fn fetch_prometheus_metrics(prometheus_url: &str) -> Result<Vec<RemoteIpMetric>> {
    let client = reqwest::Client::new();

    // Prometheus クエリ - localpacketdump ジョブのメトリクスを取得
    let query =
        r#"{job="localpacketdump-rs",__name__!~".*scrape.*",__name__!="up",__name__!~".*total.*"}"#;
    let url = format!(
        "{}api/v1/query?query={}",
        prometheus_url,
        urlencoding::encode(query)
    );

    let response = client.get(&url).send().await?;
    let json: Value = response.json().await?;

    let mut metrics_list: Vec<RemoteIpMetric> = Vec::new();

    if let Some(result) = json["data"]["result"].as_array() {
        for item in result {
            if let (Some(metric), Some(value)) =
                (item["metric"].as_object(), item["value"].as_array())
            {
                let remote_ip = metric
                    .get("remote_ip")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let interface = metric
                    .get("interface")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let metric_name = metric
                    .get("__name__")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                let metric_value: u64 = value
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);

                let data_type = match metric_name {
                    "download_bytes" => "download",
                    "upload_bytes" => "upload",
                    _ => continue,
                };

                // データ量が100バイト以下の場合はスキップ
                if metric_value <= 100 {
                    continue;
                }

                metrics_list.push(RemoteIpMetric {
                    ip: remote_ip,
                    interface,
                    data_type: data_type.to_string(),
                    bytes: metric_value,
                });
            }
        }
    }

    Ok(metrics_list)
}

async fn measure_icmp_rtt(target_ip: &str) -> Option<f64> {
    use std::process::Command;

    // macOS では `ping` コマンドを使用（1回のみ、1秒のタイムアウト）
    let output = Command::new("ping")
        .arg("-c")
        .arg("1")
        .arg("-W")
        .arg("1000")
        .arg(target_ip)
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // "time=42.123 ms" の形式を抽出
            for line in stdout.lines() {
                if let Some(start) = line.find("time=") {
                    let rest = &line[start + 5..];
                    if let Some(end) = rest.find(" ms") {
                        if let Ok(rtt) = rest[..end].parse::<f64>() {
                            return Some(rtt);
                        }
                    }
                }
            }
            None
        }
        Err(e) => {
            error!("Failed to run ping: {}", e);
            None
        }
    }
}

async fn ping_and_update_metrics(
    metrics: Arc<MetricsCollector>,
    remote_metrics: Vec<RemoteIpMetric>,
) {
    // 各メトリクスに対して並列で ICMP ping を実行
    let handles: Vec<_> = remote_metrics
        .iter()
        .map(|metric| {
            let ip = metric.ip.clone();
            let interface = metric.interface.clone();
            let data_type = metric.data_type.clone();
            let metrics = Arc::clone(&metrics);

            task::spawn(async move {
                if let Some(rtt) = measure_icmp_rtt(&ip).await {
                    metrics.set_rtt(&ip, &interface, &data_type, rtt);
                    info!(
                        "Measured RTT to {} on {} ({}): {:.2}ms",
                        ip, interface, data_type, rtt
                    );
                }
            })
        })
        .collect();

    // すべてのタスクが完了するまで待つ
    for handle in handles {
        let _ = handle.await;
    }
}

async fn run_http_server(metrics: Arc<MetricsCollector>, port: u16) -> Result<()> {
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Request, Response, Server, StatusCode};

    let metrics_clone = Arc::clone(&metrics);

    let make_svc = make_service_fn(move |_conn| {
        let metrics = Arc::clone(&metrics_clone);
        async move {
            Ok::<_, hyper::Error>(service_fn(move |_req: Request<Body>| {
                let metrics = Arc::clone(&metrics);
                async move {
                    match metrics.gather_metrics() {
                        Ok(body) => Ok::<_, hyper::Error>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("Content-Type", "text/plain; version=0.0.4")
                                .body(Body::from(body))
                                .unwrap(),
                        ),
                        Err(_) => Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Body::from("Error gathering metrics"))
                            .unwrap()),
                    }
                }
            }))
        }
    });

    let addr = ([127, 0, 0, 1], port).into();
    let server = Server::bind(&addr).serve(make_svc);

    info!("Metrics server listening on http://{}", addr);
    server.await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // ログ初期化
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let prometheus_url = "http://localhost:9090/";
    let exporter_port = 59123;

    let metrics = Arc::new(MetricsCollector::new()?);

    // HTTP サーバーをバックグラウンドで起動
    let server_metrics = Arc::clone(&metrics);
    let _server_handle = tokio::spawn(async move {
        if let Err(e) = run_http_server(server_metrics, exporter_port).await {
            error!("Server error: {}", e);
        }
    });

    // メインループ：定期的に Prometheus からデータを取得して ICMP ping を実行
    loop {
        match fetch_prometheus_metrics(prometheus_url).await {
            Ok(remote_metrics) => {
                info!(
                    "Fetched {} metrics from Prometheus (filtered by >100 bytes)",
                    remote_metrics.len()
                );
                for metric in &remote_metrics {
                    info!(
                        "IP: {}, Interface: {}, Type: {}, Bytes: {}",
                        metric.ip, metric.interface, metric.data_type, metric.bytes
                    );
                }

                // ICMP ping を実行してメトリクスを更新
                ping_and_update_metrics(Arc::clone(&metrics), remote_metrics).await;
            }
            Err(e) => {
                error!("Failed to fetch Prometheus metrics: {}", e);
            }
        }

        // スクレイプ間隔は 1 秒（Prometheus の設定に合わせる）
        sleep(Duration::from_secs(1)).await;
    }
}
