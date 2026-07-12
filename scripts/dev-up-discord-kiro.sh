#!/usr/bin/env bash
# Local k8s: Discord adapter with bundled kiro-cli agent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/_lib.sh
source "${SCRIPT_DIR}/_lib.sh"

PROFILE="discord-kiro"
IMAGE="${IMAGE:-localhost:5555/openab:kiro-dev}"
BUILD=0
DELETE=0

usage() {
  cat <<USAGE
Usage:
  scripts/dev-up-discord-kiro.sh [--build] [--image <image>]
  scripts/dev-up-discord-kiro.sh --delete

Deploys openab-discord-kiro — Discord + kiro-cli (OPENAB_AGENT_COMMAND from image).

Secrets:
  openab-discord     — DISCORD_BOT_TOKEN
  openab-kiro-api    — KIRO_API_KEY
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build) BUILD=1; shift ;;
    --image) IMAGE="${2:?}"; shift 2 ;;
    --image=*) IMAGE="${1#*=}"; shift ;;
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
  IMAGE=$(oab_build_image kiro | sed -n 's/^image=//p' | tail -1)
fi

oab_ensure_secret_literal openab-discord \
  --from-literal=DISCORD_BOT_TOKEN=DISCORD_TOKEN_PLACEHOLDER
oab_ensure_secret_literal openab-kiro-api \
  --from-literal=KIRO_API_KEY=KIRO_API_KEY_PLACEHOLDER

OAB_PROFILE="$PROFILE"
OAB_IMAGE="$IMAGE"
OAB_AGENT_CONFIG='[agent]
env = { KIRO_API_KEY = "${KIRO_API_KEY}" }

[discord]
bot_token = "${DISCORD_BOT_TOKEN}"

[pool]
max_sessions = 3
session_ttl_hours = 1'
OAB_EXTRA_ENV_YAML=$(cat <<'YAML'
            - name: DISCORD_BOT_TOKEN
              valueFrom:
                secretKeyRef:
                  name: openab-discord
                  key: DISCORD_BOT_TOKEN
            - name: KIRO_API_KEY
              valueFrom:
                secretKeyRef:
                  name: openab-kiro-api
                  key: KIRO_API_KEY
YAML
)

oab_deploy_profile "$PROFILE"
oab_print_discord_next_steps "$PROFILE"