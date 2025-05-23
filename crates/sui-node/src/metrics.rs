// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use mysten_network::metrics::MetricsCallbackProvider;
use prometheus::{
    register_histogram_vec_with_registry, register_int_counter_vec_with_registry,
    register_int_gauge_vec_with_registry, register_int_gauge_with_registry, HistogramVec,
    IntCounterVec, IntGauge, IntGaugeVec, Registry,
};

use std::time::Duration;
use sui_network::tonic::Code;

pub struct SuiNodeMetrics {
    pub jwk_requests: IntCounterVec,
    pub jwk_request_errors: IntCounterVec,

    pub total_jwks: IntCounterVec,
    pub invalid_jwks: IntCounterVec,
    pub unique_jwks: IntCounterVec,

    pub current_protocol_version: IntGauge,
    pub binary_max_protocol_version: IntGauge,
    pub configured_max_protocol_version: IntGauge,
}

impl SuiNodeMetrics {
    pub fn new(registry: &Registry) -> Self {
        Self {
            jwk_requests: register_int_counter_vec_with_registry!(
                "jwk_requests",
                "Total number of JWK requests",
                &["provider"],
                registry,
            )
            .unwrap(),
            jwk_request_errors: register_int_counter_vec_with_registry!(
                "jwk_request_errors",
                "Total number of JWK request errors",
                &["provider"],
                registry,
            )
            .unwrap(),
            total_jwks: register_int_counter_vec_with_registry!(
                "total_jwks",
                "Total number of JWKs",
                &["provider"],
                registry,
            )
            .unwrap(),
            invalid_jwks: register_int_counter_vec_with_registry!(
                "invalid_jwks",
                "Total number of invalid JWKs",
                &["provider"],
                registry,
            )
            .unwrap(),
            unique_jwks: register_int_counter_vec_with_registry!(
                "unique_jwks",
                "Total number of unique JWKs",
                &["provider"],
                registry,
            )
            .unwrap(),
            current_protocol_version: register_int_gauge_with_registry!(
                "sui_current_protocol_version",
                "Current protocol version in this epoch",
                registry,
            )
            .unwrap(),
            binary_max_protocol_version: register_int_gauge_with_registry!(
                "sui_binary_max_protocol_version",
                "Max protocol version supported by this binary",
                registry,
            )
            .unwrap(),
            configured_max_protocol_version: register_int_gauge_with_registry!(
                "sui_configured_max_protocol_version",
                "Max protocol version configured in the node config",
                registry,
            )
            .unwrap(),
        }
    }
}

#[derive(Clone)]
pub struct GrpcMetrics {
    inflight_grpc: IntGaugeVec,
    grpc_requests: IntCounterVec,
    grpc_request_latency: HistogramVec,
}

const LATENCY_SEC_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1., 2.5, 5., 10., 20., 30., 60., 90.,
];

impl GrpcMetrics {
    pub fn new(registry: &Registry) -> Self {
        Self {
            inflight_grpc: register_int_gauge_vec_with_registry!(
                "inflight_grpc",
                "Total in-flight GRPC requests per route",
                &["path"],
                registry,
            )
            .unwrap(),
            grpc_requests: register_int_counter_vec_with_registry!(
                "grpc_requests",
                "Total GRPC requests per route",
                &["path", "status"],
                registry,
            )
            .unwrap(),
            grpc_request_latency: register_histogram_vec_with_registry!(
                "grpc_request_latency",
                "Latency of GRPC requests per route",
                &["path"],
                LATENCY_SEC_BUCKETS.to_vec(),
                registry,
            )
            .unwrap(),
        }
    }
}

impl MetricsCallbackProvider for GrpcMetrics {
    fn on_request(&self, _path: String) {}

    fn on_response(&self, path: String, latency: Duration, _status: u16, grpc_status_code: Code) {
        self.grpc_requests
            .with_label_values(&[path.as_str(), format!("{grpc_status_code:?}").as_str()])
            .inc();
        self.grpc_request_latency
            .with_label_values(&[path.as_str()])
            .observe(latency.as_secs_f64());
    }

    fn on_start(&self, path: &str) {
        self.inflight_grpc.with_label_values(&[path]).inc();
    }

    fn on_drop(&self, path: &str) {
        self.inflight_grpc.with_label_values(&[path]).dec();
    }
}

#[cfg(test)]
mod tests {
    use mysten_metrics::start_prometheus_server;
    use prometheus::{IntCounter, Registry};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[tokio::test]
    pub async fn test_metrics_endpoint_with_multiple_registries_add_remove() {
        let port: u16 = 8081;
        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);

        let registry_service = start_prometheus_server(socket);

        tokio::task::yield_now().await;

        // now add a few registries to the service along side with metrics
        let registry_1 = Registry::new_custom(Some("narwhal".to_string()), None).unwrap();
        let counter_1 = IntCounter::new("counter_1", "a sample counter 1").unwrap();
        registry_1.register(Box::new(counter_1)).unwrap();

        let registry_2 = Registry::new_custom(Some("sui".to_string()), None).unwrap();
        let counter_2 = IntCounter::new("counter_2", "a sample counter 2").unwrap();
        registry_2.register(Box::new(counter_2.clone())).unwrap();

        let registry_1_id = registry_service.add(registry_1);
        let _registry_2_id = registry_service.add(registry_2);

        // request the endpoint
        let result = get_metrics(port).await;

        assert!(result.contains(
            "# HELP sui_counter_2 a sample counter 2
# TYPE sui_counter_2 counter
sui_counter_2 0"
        ));

        assert!(result.contains(
            "# HELP narwhal_counter_1 a sample counter 1
# TYPE narwhal_counter_1 counter
narwhal_counter_1 0"
        ));

        // Now remove registry 1
        assert!(registry_service.remove(registry_1_id));

        // AND increase metric 2
        counter_2.inc();

        // Now pull again metrics
        // request the endpoint
        let result = get_metrics(port).await;

        // Registry 1 metrics should not be present anymore
        assert!(!result.contains(
            "# HELP narwhal_counter_1 a sample counter 1
# TYPE narwhal_counter_1 counter
narwhal_counter_1 0"
        ));

        // Registry 2 metric should have increased by 1
        assert!(result.contains(
            "# HELP sui_counter_2 a sample counter 2
# TYPE sui_counter_2 counter
sui_counter_2 1"
        ));
    }

    async fn get_metrics(port: u16) -> String {
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://127.0.0.1:{}/metrics", port))
            .send()
            .await
            .unwrap();
        response.text().await.unwrap()
    }
}
