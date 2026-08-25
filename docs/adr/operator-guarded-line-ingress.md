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
metrics. A one-minute route-level `4xx >= 1` CloudWatch alarm is the bounded
pilot stop signal. AWS HTTP APIs do not expose an exact native `429` metric, so
this alarm is intentionally conservative: other 4xx responses can trigger it,
and AWS documents that some excessive throttling responses might not emit
metrics.

Removing the guard while retaining ingress clears the route settings and
deletes the alarm. Removing ingress or deleting the service also deletes the
alarm as part of best-effort ingress teardown. The ECS service-registry
reconciler compares the ARN, container name, and container port so enabling or
disabling the guard repoints an existing service correctly.

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
stop-signal alarm are the approved bounded controls. Stronger edge identity and
a dedicated exact-429 telemetry path remain future work before expanding the
pilot.

## 5. Consequences

- Existing deployments remain backwards compatible by default.
- Enabling or disabling the guard causes an ECS rolling replacement because the
  Task definition and service-registry target change.
- Operators need `cloudwatch:PutMetricAlarm` and
  `cloudwatch:DeleteAlarms` in addition to the existing ingress permissions.
- The alarm favors early stopping over specificity and must not be interpreted
  as proof that every observed 4xx was a throttle.
