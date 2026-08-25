use crate::manifest::OABServiceManifest;

/// Required API Gateway pressure controls for the guarded LINE pilot.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayControlsPlan {
    pub route_key: String,
    pub throttling_rate_limit: f64,
    pub throttling_burst_limit: i32,
    pub detailed_metrics_enabled: bool,
    pub alarm_name: String,
    pub access_log_group_name: String,
    pub access_log_retention_days: i32,
    pub access_log_format: &'static str,
    pub metric_filter_name: String,
    pub metric_filter_pattern: &'static str,
    pub metric_namespace: &'static str,
    pub metric_name: String,
    pub alarm_threshold: f64,
}

impl GatewayControlsPlan {
    pub fn alarm_name(namespace: &str, name: &str) -> String {
        format!("oab-webhook-{namespace}-{name}-line-429")
    }

    pub fn access_log_group_name(namespace: &str, name: &str) -> String {
        format!("/aws/apigateway/oab-webhook-{namespace}-{name}")
    }

    pub fn from_manifest(manifest: &OABServiceManifest) -> Option<Self> {
        let ingress = manifest.spec.ingress.as_ref()?;
        ingress.task_ingress_guard.as_ref()?;
        Some(Self {
            route_key: "POST /webhook/line".to_string(),
            throttling_rate_limit: 5.0,
            throttling_burst_limit: 10,
            detailed_metrics_enabled: true,
            alarm_name: Self::alarm_name(&manifest.metadata.namespace, &manifest.metadata.name),
            access_log_group_name: Self::access_log_group_name(
                &manifest.metadata.namespace,
                &manifest.metadata.name,
            ),
            access_log_retention_days: 7,
            access_log_format: r#"{"requestId":"$context.requestId","routeKey":"$context.routeKey","status":$context.status}"#,
            metric_filter_name: Self::alarm_name(
                &manifest.metadata.namespace,
                &manifest.metadata.name,
            ),
            metric_filter_pattern: r#"{ ($.status = 429) && ($.routeKey = "POST /webhook/line") }"#,
            metric_namespace: "OAB/Pilot",
            metric_name: Self::alarm_name(&manifest.metadata.namespace, &manifest.metadata.name),
            alarm_threshold: 1.0,
        })
    }
}

/// Desired reconciliation when a manifest adds, retains, or removes the
/// guarded LINE ingress controls.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayControlsTransition {
    Ensure(GatewayControlsPlan),
    Remove {
        alarm_name: String,
        access_log_group_name: String,
        metric_filter_name: String,
    },
    Unchanged,
}

impl GatewayControlsTransition {
    pub fn from_manifest(previously_had_guard: bool, manifest: &OABServiceManifest) -> Self {
        if let Some(plan) = GatewayControlsPlan::from_manifest(manifest) {
            return Self::Ensure(plan);
        }
        if previously_had_guard {
            return Self::Remove {
                alarm_name: GatewayControlsPlan::alarm_name(
                    &manifest.metadata.namespace,
                    &manifest.metadata.name,
                ),
                access_log_group_name: GatewayControlsPlan::access_log_group_name(
                    &manifest.metadata.namespace,
                    &manifest.metadata.name,
                ),
                metric_filter_name: GatewayControlsPlan::alarm_name(
                    &manifest.metadata.namespace,
                    &manifest.metadata.name,
                ),
            };
        }
        Self::Unchanged
    }
}
