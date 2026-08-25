use oabctl::{GatewayControlsPlan, OABServiceManifest};

#[test]
fn guarded_line_manifest_plans_route_throttle_and_conservative_429_alarm() {
    let manifest: OABServiceManifest = serde_yaml::from_str(
        r#"
apiVersion: oab.dev/v2
kind: OABService
metadata:
  name: line-pilot
  namespace: prod
spec:
  image: public.ecr.aws/oab/openab@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  resources:
    cpu: "256"
    memory: "512"
  configFrom: s3://bucket/config.toml
  runtime:
    type: ecs
    capacityProvider: FARGATE_SPOT
    networking:
      subnets: ["subnet-a"]
      securityGroups: ["sg-1"]
  ingress:
    paths: ["/webhook/line"]
    taskIngressGuard:
      image: public.ecr.aws/oab/task-ingress-guard@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
"#,
    )
    .expect("parse guarded manifest");
    manifest.validate().expect("validate guarded manifest");

    let plan = GatewayControlsPlan::from_manifest(&manifest).expect("gateway controls plan");

    assert_eq!(plan.route_key, "POST /webhook/line");
    assert_eq!(plan.throttling_rate_limit, 5.0);
    assert_eq!(plan.throttling_burst_limit, 10);
    assert!(plan.detailed_metrics_enabled);
    assert_eq!(plan.alarm_name, "oab-webhook-prod-line-pilot-line-4xx");
    assert_eq!(plan.metric_name, "4xx");
    assert_eq!(plan.method, "POST");
    assert_eq!(plan.resource, "/webhook/line");
    assert_eq!(plan.stage, "prod");
    assert_eq!(plan.alarm_threshold, 1.0);
}
