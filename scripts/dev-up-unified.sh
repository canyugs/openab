#!/usr/bin/env bash
# Local k8s: unified OpenAB with WeCom + Google Chat webhooks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/_lib.sh
source "${SCRIPT_DIR}/_lib.sh"

PROFILE="unified"
IMAGE="${IMAGE:-localhost:5555/openab:agentcore-dev}"
BUILD=0
DELETE=0
GATEWAY_ALLOW_ALL_USERS="${GATEWAY_ALLOW_ALL_USERS:-false}"

usage() {
  cat <<USAGE
Usage:
  scripts/dev-up-unified.sh [--build] [--image <image>] [--allow-all-users]
  scripts/dev-up-unified.sh --delete

Deploys openab-unified (WeCom + Google Chat embedded webhooks, echo agent).
Same as the original dev-deploy-unified-k8s.sh but uses the shared profile lib.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build) BUILD=1; shift ;;
    --image) IMAGE="${2:?}"; shift 2 ;;
    --image=*) IMAGE="${1#*=}"; shift ;;
    --allow-all-users) GATEWAY_ALLOW_ALL_USERS=true; shift ;;
    --delete) DELETE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) oab_die "unknown argument: $1" ;;
  esac
done

if [[ "$DELETE" == "1" ]]; then
  oab_need kubectl
  oab_check_context
  oab_delete_profile "$PROFILE"
  exit 0
fi

if [[ "$BUILD" == "1" ]]; then
  IMAGE=$(oab_build_image agentcore unified-dev | sed -n 's/^image=//p' | tail -1)
fi

oab_ensure_secret_literal openab-platform \
  --from-literal=WECOM_CORP_ID=ww_test_corp \
  --from-literal=WECOM_AGENT_ID=1000002 \
  --from-literal=WECOM_SECRET=test_secret \
  --from-literal=WECOM_TOKEN=test_token \
  --from-literal=WECOM_ENCODING_AES_KEY=abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG

OAB_PROFILE="$PROFILE"
OAB_IMAGE="$IMAGE"
OAB_WEBHOOK_PORT=8080
OAB_AGENT_CONFIG='[agent]
command = "/bin/echo"
args = ["ok"]
working_dir = "/home/agent"

[pool]
max_sessions = 3
session_ttl_hours = 1'
OAB_EXTRA_ENV_YAML=$(cat <<YAML
            - name: GOOGLE_CHAT_ENABLED
              value: "true"
            - name: GATEWAY_LISTEN
              value: "0.0.0.0:8080"
            - name: GATEWAY_ALLOW_ALL_USERS
              value: "${GATEWAY_ALLOW_ALL_USERS}"
            - name: WECOM_CORP_ID
              valueFrom:
                secretKeyRef:
                  name: openab-platform
                  key: WECOM_CORP_ID
            - name: WECOM_AGENT_ID
              valueFrom:
                secretKeyRef:
                  name: openab-platform
                  key: WECOM_AGENT_ID
            - name: WECOM_SECRET
              valueFrom:
                secretKeyRef:
                  name: openab-platform
                  key: WECOM_SECRET
            - name: WECOM_TOKEN
              valueFrom:
                secretKeyRef:
                  name: openab-platform
                  key: WECOM_TOKEN
            - name: WECOM_ENCODING_AES_KEY
              valueFrom:
                secretKeyRef:
                  name: openab-platform
                  key: WECOM_ENCODING_AES_KEY
YAML
)

oab_deploy_profile "$PROFILE"
oab_print_gateway_next_steps "$PROFILE"