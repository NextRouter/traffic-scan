use anyhow::{Context, Result};
use lazy_static::lazy_static;
use log::{error, info, warn};
use prometheus::{Encoder, Gauge, Opts, Registry, TextEncoder};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Prometheusのクエリレスポンス構造
#[derive(Debug, Deserialize)]
struct PrometheusResponse {
    status: String,
    data: PrometheusData,
}

#[derive(Debug, Deserialize)]
struct PrometheusData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: Vec<PrometheusResult>,
}

#[derive(Debug, Deserialize, Clone)]
struct PrometheusResult {
    metric: HashMap<String, String>,
    value: (f64, String),
}

// スループットメトリクスのキー
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct MetricKey {
    interface: String,
    remote_ip: String,
}

lazy_static! {
    static ref REGISTRY: Registry = Registry::new();
    static ref THROUGHPUT_GAUGES: Arc<Mutex<HashMap<MetricKey, Gauge>>> =
        Arc::new(Mutex::new(HashMap::new()));
    static ref THROUGHPUT_TOTAL_GAUGES: Arc<Mutex<HashMap<String, Gauge>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

struct ThroughputCalculator {
    prometheus_url: String,
    client: Client,
}

impl ThroughputCalculator {
    fn new(prometheus_url: String) -> Self {
        Self {
            prometheus_url,
            client: Client::new(),
        }
    }

    // Prometheusからメトリクスを取得
    async fn query_prometheus(&self, query: &str) -> Result<Vec<PrometheusResult>> {
        let url = format!("{}/api/v1/query", self.prometheus_url);
        let response = self
            .client
            .get(&url)
            .query(&[("query", query)])
            .send()
            .await
            .context("Failed to send request to Prometheus")?;

        let prom_response: PrometheusResponse = response
            .json()
            .await
            .context("Failed to parse Prometheus response")?;

        if prom_response.status != "success" {
            anyhow::bail!("Prometheus query failed: {:?}", prom_response.status);
        }

        Ok(prom_response.data.result)
    }

    // メトリクスを取得して計算
    async fn calculate_throughput(&self) -> Result<()> {
        info!("Fetching metrics from Prometheus...");

        // 各メトリクスを取得
        let rtt_results = self
            .query_prometheus("rtt_icmp_dump")
            .await
            .context("Failed to query rtt_icmp_dump")?;
        let download_results = self
            .query_prometheus("download_bytes")
            .await
            .context("Failed to query download_bytes")?;
        let upload_results = self
            .query_prometheus("upload_bytes")
            .await
            .context("Failed to query upload_bytes")?;

        info!(
            "Fetched {} RTT, {} download, {} upload metrics",
            rtt_results.len(),
            download_results.len(),
            upload_results.len()
        );

        // メトリクスをinterface+remote_ipでグループ化
        let mut rtt_map: HashMap<MetricKey, f64> = HashMap::new();
        let mut download_map: HashMap<MetricKey, f64> = HashMap::new();
        let mut upload_map: HashMap<MetricKey, f64> = HashMap::new();

        for result in rtt_results {
            if let (Some(interface), Some(remote_ip)) = (
                result.metric.get("interface"),
                result.metric.get("remote_ip"),
            ) {
                let key = MetricKey {
                    interface: interface.clone(),
                    remote_ip: remote_ip.clone(),
                };
                let value: f64 = result.value.1.parse().unwrap_or(0.0);
                rtt_map.insert(key, value);
            }
        }

        for result in download_results {
            if let (Some(interface), Some(remote_ip)) = (
                result.metric.get("interface"),
                result.metric.get("remote_ip"),
            ) {
                let key = MetricKey {
                    interface: interface.clone(),
                    remote_ip: remote_ip.clone(),
                };
                let value: f64 = result.value.1.parse().unwrap_or(0.0);
                download_map.insert(key, value);
            }
        }

        for result in upload_results {
            if let (Some(interface), Some(remote_ip)) = (
                result.metric.get("interface"),
                result.metric.get("remote_ip"),
            ) {
                let key = MetricKey {
                    interface: interface.clone(),
                    remote_ip: remote_ip.clone(),
                };
                let value: f64 = result.value.1.parse().unwrap_or(0.0);
                upload_map.insert(key, value);
            }
        }

        // スループット計算: (download_bytes + upload_bytes) / rtt_icmp_dump
        let mut gauges = THROUGHPUT_GAUGES.lock().unwrap();
        let mut interface_totals: HashMap<String, f64> = HashMap::new();

        for (key, rtt) in &rtt_map {
            // 同じキーのdownloadとuploadを取得
            let download = download_map.get(key).copied().unwrap_or(0.0);
            let upload = upload_map.get(key).copied().unwrap_or(0.0);

            // RTTが0の場合はスキップ
            if *rtt <= 0.0 {
                warn!(
                    "Skipping calculation for interface={}, remote_ip={}: RTT is {}",
                    key.interface, key.remote_ip, rtt
                );
                continue;
            }

            // スループット計算
            let total_bytes = download + upload;
            let throughput = total_bytes / rtt;

            info!(
                "Calculated throughput for interface={}, remote_ip={}: ({} + {}) / {} = {}",
                key.interface, key.remote_ip, download, upload, rtt, throughput
            );

            // Gaugeを取得または作成
            let gauge = gauges.entry(key.clone()).or_insert_with(|| {
                let gauge = Gauge::with_opts(
                    Opts::new(
                        "throughputdump",
                        "Calculated throughput based on bytes and RTT",
                    )
                    .const_label("interface", &key.interface)
                    .const_label("remote_ip", &key.remote_ip)
                    .const_label("job", "throughputdump"),
                )
                .unwrap();
                REGISTRY.register(Box::new(gauge.clone())).unwrap();
                gauge
            });

            gauge.set(throughput);

            // interfaceごとのトータルに加算
            *interface_totals.entry(key.interface.clone()).or_insert(0.0) += throughput;
        }

        // interfaceごとのトータルスループットを設定
        let mut total_gauges = THROUGHPUT_TOTAL_GAUGES.lock().unwrap();
        for (interface, total_throughput) in &interface_totals {
            info!(
                "Total throughput for interface={}: {}",
                interface, total_throughput
            );

            let gauge = total_gauges.entry(interface.clone()).or_insert_with(|| {
                let gauge = Gauge::with_opts(
                    Opts::new(
                        "throughputdump_total",
                        "Total calculated throughput per interface",
                    )
                    .const_label("interface", interface)
                    .const_label("job", "throughputdump"),
                )
                .unwrap();
                REGISTRY.register(Box::new(gauge.clone())).unwrap();
                gauge
            });

            gauge.set(*total_throughput);
        }

        Ok(())
    }
}

// HTTPサーバーでメトリクスを公開
async fn serve_metrics() -> Result<()> {
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Request, Response, Server};

    let make_svc = make_service_fn(|_conn| async {
        Ok::<_, hyper::Error>(service_fn(|_req: Request<Body>| async {
            let encoder = TextEncoder::new();
            let metric_families = REGISTRY.gather();
            let mut buffer = vec![];
            encoder.encode(&metric_families, &mut buffer).unwrap();

            Response::builder()
                .status(200)
                .header("Content-Type", encoder.format_type())
                .body(Body::from(buffer))
        }))
    });

    let addr = ([0, 0, 0, 0], 59124).into();
    let server = Server::bind(&addr).serve(make_svc);

    info!("Metrics server listening on http://localhost:59124/metrics");

    server.await.context("Server error")?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let prometheus_url =
        std::env::var("PROMETHEUS_URL").unwrap_or_else(|_| "http://localhost:9090".to_string());

    info!("Starting throughput-dump");
    info!("Prometheus URL: {}", prometheus_url);

    let calculator = Arc::new(ThroughputCalculator::new(prometheus_url));

    // メトリクス更新タスク
    let calculator_clone = calculator.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if let Err(e) = calculator_clone.calculate_throughput().await {
                error!("Error calculating throughput: {}", e);
            }
        }
    });

    // メトリクスサーバー起動
    serve_metrics().await?;

    Ok(())
}
