#!/usr/bin/env bash
# Local k8s: unified OpenAB with Google Chat webhook only.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/_lib.sh
source "${SCRIPT_DIR}/_lib.sh"

PROFILE="googlechat"
IMAGE="${IMAGE:-localhost:5555/openab:agentcore-dev}"
BUILD=0
DELETE=0
GATEWAY_ALLOW_ALL_USERS="${GATEWAY_ALLOW_ALL_USERS:-false}"

usage() {
  cat <<USAGE
Usage:
  scripts/dev-up-googlechat.sh [--build] [--image <image>] [--allow-all-users]
  scripts/dev-up-googlechat.sh --delete

Deploys openab-googlechat (embedded /webhook/googlechat, echo agent).
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
  IMAGE=$(oab_build_image agentcore | sed -n 's/^image=//p' | tail -1)
fi

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
YAML
)

oab_deploy_profile "$PROFILE"
oab_print_gateway_next_steps "$PROFILE" "/webhook/googlechat"
echo "  # smoke test (deny-all echoes request-access):"
echo '  curl -sS -X POST http://127.0.0.1:18080/webhook/googlechat \'
echo '    -H "content-type: application/json" \'
echo '    -d '"'"'{"message":{"name":"spaces/abc/messages/1","text":"hi","sender":{"name":"users/123","displayName":"Test","type":"HUMAN"}},"space":{"name":"spaces/abc"}}'"'"