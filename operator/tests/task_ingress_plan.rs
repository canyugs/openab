use oabctl::{OABServiceManifest, TaskIngressPlan};

#[test]
fn guarded_line_manifest_plans_loopback_openab_and_external_guard() {
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

    let plan = TaskIngressPlan::from_manifest(&manifest).expect("task ingress plan");

    assert_eq!(
        plan,
        TaskIngressPlan::Guarded {
            external_port: 8080,
            guard_image: "public.ecr.aws/oab/task-ingress-guard@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            openab_listen: "127.0.0.1:18080".into(),
            guard_listen: "0.0.0.0:8080".into(),
            guard_upstream: "http://127.0.0.1:18080/webhook/line".into(),
        }
    );
    assert_eq!(
        plan.openab_environment(),
        vec![("GATEWAY_LISTEN", "127.0.0.1:18080")]
    );
    assert_eq!(
        plan.guard_environment(),
        Some(vec![
            ("OPENAB_TASK_INGRESS_LISTEN", "0.0.0.0:8080"),
            (
                "OPENAB_TASK_INGRESS_UPSTREAM",
                "http://127.0.0.1:18080/webhook/line"
            ),
        ])
    );
    assert_eq!(plan.external_container_name(), "task-ingress-guard");
}

#[test]
fn guarded_line_manifest_avoids_external_port_collision_on_loopback() {
    let manifest: OABServiceManifest = serde_yaml::from_str(
        r#"
apiVersion: oab.dev/v2
kind: OABService
metadata:
  name: line-pilot
  namespace: prod
spec:
  image: public.ecr.aws/oab/openab@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  resources: { cpu: "256", memory: "512" }
  configFrom: s3://bucket/config.toml
  runtime:
    type: ecs
    capacityProvider: FARGATE_SPOT
    networking:
      subnets: ["subnet-a"]
      securityGroups: ["sg-1"]
  ingress:
    containerPort: 18080
    paths: ["/webhook/line"]
    taskIngressGuard:
      image: public.ecr.aws/oab/task-ingress-guard@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
"#,
    )
    .expect("parse guarded manifest");
    manifest.validate().expect("validate guarded manifest");

    let plan = TaskIngressPlan::from_manifest(&manifest).expect("task ingress plan");

    assert!(matches!(
        plan,
        TaskIngressPlan::Guarded {
            external_port: 18080,
            ref openab_listen,
            ref guard_listen,
            ref guard_upstream,
            ..
        } if openab_listen == "127.0.0.1:18081"
            && guard_listen == "0.0.0.0:18080"
            && guard_upstream == "http://127.0.0.1:18081/webhook/line"
    ));
}
