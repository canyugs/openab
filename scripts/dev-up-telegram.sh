#!/usr/bin/env bash
# Local k8s: unified OpenAB with Telegram webhook only.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/_lib.sh
source "${SCRIPT_DIR}/_lib.sh"

PROFILE="telegram"
IMAGE="${IMAGE:-localhost:5555/openab:agentcore-dev}"
BUILD=0
DELETE=0
GATEWAY_ALLOW_ALL_USERS="${GATEWAY_ALLOW_ALL_USERS:-false}"

usage() {
  cat <<USAGE
Usage:
  scripts/dev-up-telegram.sh [--build] [--image <image>] [--allow-all-users]
  scripts/dev-up-telegram.sh --delete

Deploys openab-telegram (embedded /webhook/telegram, echo agent).

Secret openab-telegram-platform:
  TELEGRAM_BOT_TOKEN (required for live tests)
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

oab_ensure_secret_literal openab-telegram-platform \
  --from-literal=TELEGRAM_BOT_TOKEN=000000000:TEST_TOKEN_PLACEHOLDER

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
            - name: GATEWAY_LISTEN
              value: "0.0.0.0:8080"
            - name: GATEWAY_ALLOW_ALL_USERS
              value: "${GATEWAY_ALLOW_ALL_USERS}"
            - name: TELEGRAM_BOT_TOKEN
              valueFrom:
                secretKeyRef:
                  name: openab-telegram-platform
                  key: TELEGRAM_BOT_TOKEN
YAML
)

oab_deploy_profile "$PROFILE"
oab_print_gateway_next_steps "$PROFILE"
echo "  # register webhook after tunnel is up:"
echo '  curl "https://api.telegram.org/bot<token>/setWebhook?url=https://<tunnel>/webhook/telegram"'