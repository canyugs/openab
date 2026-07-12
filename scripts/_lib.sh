# Shared helpers for local OpenAB k8s dev scripts.
# Source from profile scripts: source "$(dirname "$0")/_lib.sh"

oab_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd
}

oab_die() { echo "error: $*" >&2; exit 1; }

oab_need() {
  command -v "$1" >/dev/null 2>&1 || oab_die "missing required command: $1"
}

oab_check_context() {
  [[ "${OAB_CHECK_CONTEXT:-1}" == "1" ]] || return 0
  local ctx
  ctx=$(kubectl config current-context)
  [[ "$ctx" == "${OAB_KUBE_CONTEXT:-docker-desktop}" ]] ||
    oab_die "kubectl context is '$ctx', expected '${OAB_KUBE_CONTEXT:-docker-desktop}'"
}

oab_ensure_namespace() {
  kubectl create namespace "${OAB_KUBE_NAMESPACE:-openab-local}" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
}

oab_delete_profile() {
  local profile="$1"
  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" delete deployment "openab-${profile}" --ignore-not-found
  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" delete service "openab-${profile}" --ignore-not-found
  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" delete configmap "openab-${profile}-config" --ignore-not-found
}

oab_ensure_secret_literal() {
  local name="$1"
  shift
  if kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" get secret "$name" >/dev/null 2>&1; then
    return 0
  fi
  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" create secret generic "$name" "$@" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  echo "created placeholder secret $name (replace for real credentials)"
}

# Deploy one OpenAB profile.
# Required env before call:
#   OAB_PROFILE        — short name (wecom, googlechat, discord-kiro, …)
#   OAB_IMAGE          — container image reference
# Optional:
#   OAB_CONFIG_NAME    — configmap name (default: openab-${OAB_PROFILE}-config)
#   OAB_AGENT_CONFIG   — config.toml body (heredoc string)
#   OAB_EXTRA_ENV_YAML — extra container env entries (YAML list items)
#   OAB_WEBHOOK_PORT   — set to enable Service + containerPort (gateway profiles)
#   OAB_WAIT_ROLLOUT   — 1 (default) or 0
oab_deploy_profile() {
  local profile="$1"
  local deploy="openab-${profile}"
  local service="openab-${profile}"
  local configmap="${OAB_CONFIG_NAME:-openab-${profile}-config}"
  local agent_config="${OAB_AGENT_CONFIG:-}"
  local extra_env="${OAB_EXTRA_ENV_YAML:-}"
  local webhook_port="${OAB_WEBHOOK_PORT:-}"
  local wait="${OAB_WAIT_ROLLOUT:-1}"

  [[ -n "$profile" ]] || oab_die "OAB_PROFILE is required"
  [[ -n "${OAB_IMAGE:-}" ]] || oab_die "OAB_IMAGE is required"
  [[ -n "$agent_config" ]] || oab_die "OAB_AGENT_CONFIG is required"

  oab_need kubectl
  oab_check_context
  oab_ensure_namespace

  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" apply -f - >/dev/null <<CM
apiVersion: v1
kind: ConfigMap
metadata:
  name: ${configmap}
data:
  config.toml: |
$(echo "$agent_config" | sed 's/^/    /')
CM

  local ports_yaml="" service_yaml=""
  if [[ -n "$webhook_port" ]]; then
    ports_yaml=$(cat <<YAML
          ports:
            - containerPort: ${webhook_port}
              name: webhook
YAML
)
    service_yaml=$(cat <<YAML
---
apiVersion: v1
kind: Service
metadata:
  name: ${service}
spec:
  selector:
    app: openab-${profile}
  ports:
    - name: webhook
      port: ${webhook_port}
      targetPort: ${webhook_port}
YAML
)
  fi

  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ${deploy}
  labels:
    app: openab-${profile}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: openab-${profile}
  template:
    metadata:
      labels:
        app: openab-${profile}
    spec:
      containers:
        - name: openab
          image: ${OAB_IMAGE}
          imagePullPolicy: IfNotPresent
          command: ["openab", "run", "-c", "/etc/openab/config.toml"]
${ports_yaml}
          env:
${extra_env}
          volumeMounts:
            - name: config
              mountPath: /etc/openab
              readOnly: true
      volumes:
        - name: config
          configMap:
            name: ${configmap}
${service_yaml}
YAML

  if [[ "$wait" == "1" ]]; then
    kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" rollout status "deployment/${deploy}" --timeout=180s
  fi

  kubectl -n "${OAB_KUBE_NAMESPACE:-openab-local}" get pods -l "app=openab-${profile}" \
    -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[0].ready,STATUS:.status.phase
}

oab_print_gateway_next_steps() {
  local profile="$1"
  local webhook_path="${2:-/webhook/${profile}}"
  local port="${3:-8080}"
  local pf_port="${4:-18080}"
  local ns="${OAB_KUBE_NAMESPACE:-openab-local}"
  echo ""
  echo "Deployed openab-${profile} to namespace ${ns}"
  echo ""
  echo "Next:"
  echo "  kubectl -n ${ns} port-forward svc/openab-${profile} ${pf_port}:${port}"
  echo "  curl http://127.0.0.1:${pf_port}/health"
  echo "  # public tunnel (from openab-control-plane):"
  echo "  ORIGIN_URL=http://openab-${profile}:${port} WEBHOOK_PATH=${webhook_path} \\"
  echo "    ../openab-control-plane/scripts/dev-tunnel-k8s.sh --namespace ${ns}"
}

oab_print_discord_next_steps() {
  local profile="$1"
  local ns="${OAB_KUBE_NAMESPACE:-openab-local}"
  echo ""
  echo "Deployed openab-${profile} to namespace ${ns}"
  echo ""
  echo "Next:"
  echo "  kubectl -n ${ns} logs -f deployment/openab-${profile}"
  echo "  # replace placeholder DISCORD_BOT_TOKEN in secret openab-discord before going live"
}

oab_build_image() {
  local target="${1:-agentcore}"
  local tag="${2:-}"
  local root
  root="$(oab_root)"
  "${root}/scripts/dev-build-image.sh" --target "$target" ${tag:+--tag "$tag"}
}