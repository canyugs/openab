use crate::manifest::OABServiceManifest;

/// Required API Gateway pressure controls for the guarded LINE pilot.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayControlsPlan {
    pub route_key: String,
    pub throttling_rate_limit: f64,
    pub throttling_burst_limit: i32,
    pub detailed_metrics_enabled: bool,
    pub alarm_name: String,
    pub metric_name: &'static str,
    pub method: &'static str,
    pub resource: &'static str,
    pub stage: &'static str,
    pub alarm_threshold: f64,
}

impl GatewayControlsPlan {
    pub fn from_manifest(manifest: &OABServiceManifest) -> Option<Self> {
        let ingress = manifest.spec.ingress.as_ref()?;
        ingress.task_ingress_guard.as_ref()?;
        Some(Self {
            route_key: "POST /webhook/line".to_string(),
            throttling_rate_limit: 5.0,
            throttling_burst_limit: 10,
            detailed_metrics_enabled: true,
            alarm_name: format!(
                "oab-webhook-{}-{}-line-4xx",
                manifest.metadata.namespace, manifest.metadata.name
            ),
            metric_name: "4xx",
            method: "POST",
            resource: "/webhook/line",
            stage: "prod",
            alarm_threshold: 1.0,
        })
    }
}
