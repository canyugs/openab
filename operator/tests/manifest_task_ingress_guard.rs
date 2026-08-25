use oabctl::OABServiceManifest;

const OPENAB_DIGEST: &str = "public.ecr.aws/oab/openab@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GUARD_DIGEST: &str = "public.ecr.aws/oab/task-ingress-guard@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn guarded_manifest(openab_image: &str, guard_image: &str, paths: &[&str]) -> OABServiceManifest {
    let paths = paths
        .iter()
        .map(|path| format!("      - {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    serde_yaml::from_str(&format!(
        r#"
apiVersion: oab.dev/v2
kind: OABService
metadata:
  name: line-pilot
  namespace: prod
spec:
  image: {openab_image}
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
    paths:
{paths}
    taskIngressGuard:
      image: {guard_image}
"#
    ))
    .expect("parse guarded manifest")
}

#[test]
fn accepts_digest_pinned_task_ingress_guard_for_line() {
    guarded_manifest(OPENAB_DIGEST, GUARD_DIGEST, &["/webhook/line"])
        .validate()
        .expect("valid guarded LINE ingress");
}

#[test]
fn rejects_tagged_task_ingress_guard_image() {
    let error = guarded_manifest(
        OPENAB_DIGEST,
        "public.ecr.aws/oab/task-ingress-guard:latest",
        &["/webhook/line"],
    )
    .validate()
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("ingress.taskIngressGuard.image must be pinned by sha256 digest"));
}

#[test]
fn rejects_tagged_openab_image_when_task_ingress_guard_is_enabled() {
    let error = guarded_manifest(
        "public.ecr.aws/oab/openab:latest",
        GUARD_DIGEST,
        &["/webhook/line"],
    )
    .validate()
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("spec.image must be pinned by sha256 digest when taskIngressGuard is enabled"));
}

#[test]
fn rejects_task_ingress_guard_for_routes_beyond_exact_line_webhook() {
    let error = guarded_manifest(
        OPENAB_DIGEST,
        GUARD_DIGEST,
        &["/webhook/line", "/webhook/telegram"],
    )
    .validate()
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("taskIngressGuard requires ingress.paths to be exactly ['/webhook/line']"));
}
