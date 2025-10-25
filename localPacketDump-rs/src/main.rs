use axum::{response::IntoResponse, routing::get, Router};
use dashmap::DashMap;
use pnet::datalink::{self, NetworkInterface};
use pnet::packet::ethernet::{EtherTypes, EthernetPacket};
use pnet::packet::ipv4::Ipv4Packet;
use pnet::packet::ipv6::Ipv6Packet;
use pnet::packet::Packet;
use prometheus::{Encoder, IntGaugeVec, Registry, TextEncoder};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::task;
use tokio::time::Duration;
use tracing::{error, info, warn};

#[derive(Debug, Deserialize, Clone)]
struct StatusConfig {
    lan: String,
    wan0: String,
    wan1: String,
}

#[derive(Debug, Deserialize, Clone)]
struct StatusResponse {
    config: StatusConfig,
    mappings: HashMap<String, String>,
}

#[derive(Clone)]
struct TrafficMetrics {
    // Gauge of download bytes per second over the last second (inbound traffic from remote)
    download_bytes_gauge: Arc<IntGaugeVec>,
    // Gauge of upload bytes per second over the last second (outbound traffic to remote)
    upload_bytes_gauge: Arc<IntGaugeVec>,
    // Bytes observed in the current 1-second window (download), keyed by (remote IP, interface)
    window_download_bytes: Arc<DashMap<(String, String), u64>>,
    // Bytes observed in the current 1-second window (upload), keyed by (remote IP, interface)
    window_upload_bytes: Arc<DashMap<(String, String), u64>>,
    // Track all (remote IP, interface) pairs ever seen
    known_metrics: Arc<DashMap<(String, String), ()>>,
    // Registry to gather and encode metrics
    registry: Arc<Registry>,
    // Local CIDR ranges (e.g., 10.40.0.0/20) - packets from/to these IPs are considered local
    local_cidrs: Arc<Vec<ipnetwork::IpNetwork>>,
    // Current status from the external service
    status: Arc<tokio::sync::RwLock<Option<StatusResponse>>>,
    // Status endpoint URL
    status_url: String,
}

impl TrafficMetrics {
    fn new(registry: Arc<Registry>) -> Self {
        let download_bytes_gauge = IntGaugeVec::new(
            prometheus::Opts::new(
                "download_bytes",
                "Download bytes per remote IP over the last second (inbound traffic)",
            )
            .const_label("job", "localpacketdump"),
            &["remote_ip", "interface"],
        )
        .expect("failed to create download_bytes gauge");

        let upload_bytes_gauge = IntGaugeVec::new(
            prometheus::Opts::new(
                "upload_bytes",
                "Upload bytes per remote IP over the last second (outbound traffic)",
            )
            .const_label("job", "localpacketdump"),
            &["remote_ip", "interface"],
        )
        .expect("failed to create upload_bytes gauge");

        registry
            .register(Box::new(download_bytes_gauge.clone()))
            .expect("failed to register download_bytes gauge");
        registry
            .register(Box::new(upload_bytes_gauge.clone()))
            .expect("failed to register upload_bytes gauge");

        // Parse local CIDR ranges from environment variable
        // Default is 10.40.0.0/20 - adjust based on your local network
        let local_cidrs_str =
            env::var("LOCAL_CIDRS").unwrap_or_else(|_| "10.40.0.0/20".to_string());
        let local_cidrs: Vec<ipnetwork::IpNetwork> = local_cidrs_str
            .split(',')
            .filter_map(|cidr| match ipnetwork::IpNetwork::from_str(cidr.trim()) {
                Ok(net) => {
                    info!("Configured local CIDR: {}", net);
                    Some(net)
                }
                Err(e) => {
                    error!("Failed to parse local CIDR {}: {}", cidr, e);
                    None
                }
            })
            .collect();

        let status_url =
            env::var("STATUS_URL").unwrap_or_else(|_| "http://localhost:32599/status".to_string());

        Self {
            download_bytes_gauge: Arc::new(download_bytes_gauge),
            upload_bytes_gauge: Arc::new(upload_bytes_gauge),
            window_download_bytes: Arc::new(DashMap::new()),
            window_upload_bytes: Arc::new(DashMap::new()),
            known_metrics: Arc::new(DashMap::new()),
            registry,
            local_cidrs: Arc::new(local_cidrs),
            status: Arc::new(tokio::sync::RwLock::new(None)),
            status_url,
        }
    }

    async fn fetch_status(&self) {
        match reqwest::get(&self.status_url).await {
            Ok(response) => match response.json::<StatusResponse>().await {
                Ok(status) => {
                    info!(
                        "Fetched status: config={:?}, mappings={:?}",
                        status.config, status.mappings
                    );
                    *self.status.write().await = Some(status);
                }
                Err(e) => {
                    warn!("Failed to parse status response: {}", e);
                }
            },
            Err(e) => {
                warn!("Failed to fetch status from {}: {}", self.status_url, e);
            }
        }
    }

    async fn get_interface_for_ip(&self, local_ip: &str) -> String {
        let status_guard = self.status.read().await;
        if let Some(status) = status_guard.as_ref() {
            // Check if local_ip is in mappings
            if let Some(wan_name) = status.mappings.get(local_ip) {
                // wan_name is either "wan0" or "wan1"
                match wan_name.as_str() {
                    "wan0" => return status.config.wan0.clone(),
                    "wan1" => return status.config.wan1.clone(),
                    _ => {}
                }
            }
            // Not in mappings, so it's wan0
            return status.config.wan0.clone();
        }
        // Fallback if status is not available
        "unknown".to_string()
    }

    // Check if an IP address is in local CIDR range
    fn is_local_ip(&self, ip_str: &str) -> bool {
        if let Ok(ip) = IpAddr::from_str(ip_str) {
            for network in self.local_cidrs.iter() {
                if network.contains(ip) {
                    return true;
                }
            }
        }
        false
    }

    // Process a packet and record bytes based on direction
    // Download: remote source -> local destination
    // Upload: local source -> remote destination
    async fn record_packet(&self, src_ip: &str, dst_ip: &str, bytes: u64) {
        let src_is_local = self.is_local_ip(src_ip);
        let dst_is_local = self.is_local_ip(dst_ip);

        match (src_is_local, dst_is_local) {
            // Download: remote -> local
            (false, true) => {
                let interface = self.get_interface_for_ip(dst_ip).await;
                let key = (src_ip.to_string(), interface);
                self.window_download_bytes
                    .entry(key.clone())
                    .and_modify(|v| *v += bytes)
                    .or_insert(bytes);
                self.known_metrics.insert(key, ());
            }
            // Upload: local -> remote
            (true, false) => {
                let interface = self.get_interface_for_ip(src_ip).await;
                let key = (dst_ip.to_string(), interface);
                self.window_upload_bytes
                    .entry(key.clone())
                    .and_modify(|v| *v += bytes)
                    .or_insert(bytes);
                self.known_metrics.insert(key, ());
            }
            // Local -> Local or Remote -> Remote: ignore
            _ => {}
        }
    }

    // Compute bytes from the last second window, update gauges, then reset the window
    fn publish_bytes_and_reset(&self) {
        // Collect keys present in this window
        let mut current_download_keys: HashSet<(String, String)> = HashSet::new();
        let mut current_upload_keys: HashSet<(String, String)> = HashSet::new();

        // Update download_bytes gauge
        for entry in self.window_download_bytes.iter() {
            let (remote_ip, interface) = entry.key();
            let bytes = *entry.value() as i64;
            self.download_bytes_gauge
                .with_label_values(&[remote_ip, interface])
                .set(bytes);
            current_download_keys.insert((remote_ip.clone(), interface.clone()));
        }

        // Update upload_bytes gauge
        for entry in self.window_upload_bytes.iter() {
            let (remote_ip, interface) = entry.key();
            let bytes = *entry.value() as i64;
            self.upload_bytes_gauge
                .with_label_values(&[remote_ip, interface])
                .set(bytes);
            current_upload_keys.insert((remote_ip.clone(), interface.clone()));
        }

        // For known (remote_ip, interface) pairs not seen in this window, set 0
        for entry in self.known_metrics.iter() {
            let key = entry.key();
            if !current_download_keys.contains(key) {
                self.download_bytes_gauge
                    .with_label_values(&[&key.0, &key.1])
                    .set(0);
            }
            if !current_upload_keys.contains(key) {
                self.upload_bytes_gauge
                    .with_label_values(&[&key.0, &key.1])
                    .set(0);
            }
        }

        // Reset window
        self.window_download_bytes.clear();
        self.window_upload_bytes.clear();
    }

    fn encode_metrics(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = vec![];
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("failed to encode metrics");
        String::from_utf8(buffer).expect("metrics contained invalid UTF-8")
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let registry = Arc::new(Registry::new());
    let interface_name = env::var("INTERFACE_NAME").unwrap_or_else(|_| "eth2".to_string());

    let metrics = TrafficMetrics::new(registry.clone());
    let metrics_clone = metrics.clone();
    let metrics_clone_for_tick = metrics.clone();
    let metrics_clone_for_status = metrics.clone();
    let interface_name_clone = interface_name.clone();

    // Fetch status initially
    metrics.fetch_status().await;

    // Status更新タスク (10秒ごと)
    task::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            metrics_clone_for_status.fetch_status().await;
        }
    });

    // パケット監視タスクを起動
    task::spawn(async move {
        monitor_interface(metrics_clone, &interface_name_clone).await;
    });

    // 1秒ごとにバイト数を公開するタスク
    task::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            metrics_clone_for_tick.publish_bytes_and_reset();
        }
    });

    // Prometheus メトリクスエンドポイント
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:59122")
        .await
        .unwrap();

    info!("Metrics server listening on http://0.0.0.0:59122/metrics");

    axum::serve(listener, app).await.unwrap();
}

async fn metrics_handler(
    axum::extract::State(metrics): axum::extract::State<TrafficMetrics>,
) -> impl IntoResponse {
    metrics.encode_metrics()
}

async fn monitor_interface(metrics: TrafficMetrics, interface_name: &str) {
    loop {
        match get_interface_by_name(interface_name) {
            Some(interface) => {
                info!("Monitoring interface: {}", interface_name);
                let (_tx, mut rx) = match datalink::channel(&interface, Default::default()) {
                    Ok(datalink::Channel::Ethernet(tx, rx)) => (tx, rx),
                    Ok(_) => {
                        info!("Unsupported channel type for {}", interface_name);
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    Err(e) => {
                        error!("Error creating channel for {}: {}", interface_name, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                loop {
                    match rx.next() {
                        Ok(packet) => {
                            // Parse Ethernet frame first
                            if let Some(eth) = EthernetPacket::new(packet) {
                                match eth.get_ethertype() {
                                    EtherTypes::Ipv4 => {
                                        if let Some(ipv4) = Ipv4Packet::new(eth.payload()) {
                                            let src_ip = ipv4.get_source().to_string();
                                            let dst_ip = ipv4.get_destination().to_string();
                                            let packet_len = ipv4.packet().len() as u64;

                                            metrics
                                                .record_packet(&src_ip, &dst_ip, packet_len)
                                                .await;
                                        }
                                    }
                                    EtherTypes::Ipv6 => {
                                        if let Some(ipv6) = Ipv6Packet::new(eth.payload()) {
                                            let src_ip = ipv6.get_source().to_string();
                                            let dst_ip = ipv6.get_destination().to_string();
                                            let packet_len = ipv6.packet().len() as u64;

                                            metrics
                                                .record_packet(&src_ip, &dst_ip, packet_len)
                                                .await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => {
                            error!("Error receiving packet: {}", e);
                            break;
                        }
                    }
                }
            }
            None => {
                error!("Interface {} not found, retrying...", interface_name);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}

fn get_interface_by_name(name: &str) -> Option<NetworkInterface> {
    datalink::interfaces()
        .into_iter()
        .find(|interface| interface.name == name)
}
