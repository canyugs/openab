# ADR: Operator-Owned Guarded LINE Ingress

- **Status:** Accepted for bounded pilot
- **Date:** 2026-08-26

---

## 1. Context

The warm ECS runtime-routing pilot needs a narrow public ingress path for LINE
webhooks without exposing OpenAB's listener directly. Existing `OABService`
manifests already use a single-container ECS Task and API Gateway HTTP API
ingress, so changing their topology implicitly would be a compatibility and
operational risk.

The operator is the component that already owns ECS Task definitions, Cloud Map
service-registry targets, API Gateway routes, and their teardown lifecycle. It
therefore owns this ingress topology and its pressure controls rather than
requiring manual AWS changes outside desired-state reconciliation.

## 2. Decision

Add the opt-in `spec.ingress.taskIngressGuard.image` manifest field. It is valid
only when the ingress path is exactly `/webhook/line`, and both the OpenAB and
guard images must be pinned by digest.

When enabled, the operator reconciles two essential containers in one Fargate
Task:

- OpenAB binds a Task-local loopback port and has no external port mapping.
- `task-ingress-guard` binds `0.0.0.0` on the manifest's ingress port, is the
  Cloud Map service-registry target, and forwards only the exact
  `/webhook/line` path to OpenAB over loopback.

The operator also reconciles the exact API Gateway route
`POST /webhook/line` at 5 requests per second with burst 10 and detailed
metrics. Because AWS HTTP APIs expose a native aggregate `4xx` metric rather
than a distinct `429` metric, the operator enables JSON access logs on the
stage, retains them for seven days, and installs a CloudWatch Logs metric filter
that matches only status `429` on `POST /webhook/line`. A one-minute
`Sum >= 1` alarm on that per-bot custom metric is the bounded pilot stop signal.

Removing the guard while retaining ingress clears the route settings and
stage access-log settings, then deletes the alarm, metric filter, and per-bot
log group. Removing ingress or deleting the service performs the same telemetry
cleanup after the stage is gone. The ECS service-registry reconciler compares
the ARN, container name, and container port so enabling or disabling the guard
repoints an existing service correctly.

## 3. Compatibility and Ownership

Manifests without `taskIngressGuard` retain the existing single-container
topology and do not gain new pressure controls. The operator owns creation,
update, and cleanup of the guarded topology and controls; no manual AWS resource
is part of the steady-state contract.

This decision records operator support only. It does not publish either image,
deploy production resources, change LINE webhook configuration, or authorize a
pilot cutover.

## 4. Security Boundary and Accepted Risk

Loopback is the trust boundary between the guard and OpenAB inside one
`awsvpc` Task. Only the guard has an externally registered port, and both
containers are essential so a guard failure stops the Task instead of exposing
a fallback path.

The public HTTP API does not add transport-level caller identity in this pilot.
LINE signature validation, the exact-path guard, API Gateway throttling, and the
exact-429 stop-signal alarm are the approved bounded controls. The alarm is
specific to the route and response status, but it does not distinguish an API
Gateway-generated 429 from a backend 429. API Gateway throttling is also a
best-effort target rather than a guaranteed request ceiling. Stronger edge
identity remains future work before expanding the pilot.

## 5. Consequences

- Existing deployments remain backwards compatible by default.
- Enabling or disabling the guard causes an ECS rolling replacement because the
  Task definition and service-registry target change.
- Operators need CloudWatch Logs access-log delivery, retention, metric-filter,
  and cleanup permissions plus `cloudwatch:PutMetricAlarm` and
  `cloudwatch:DeleteAlarms` in addition to the existing ingress permissions.
- Each guarded bot creates one short-retention log group and one custom metric.
