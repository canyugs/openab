#!/usr/bin/env bash
# Build an OpenAB image from Dockerfile.unified for local k8s development.
set -euo pipefail

IMAGE_NAME="${IMAGE_NAME:-openab}"
TAG="${TAG:-}"
TARGET="${TARGET:-agentcore}"
REGISTRY="${REGISTRY:-localhost:5555}"
PLATFORM="${PLATFORM:-}"

usage() {
  cat <<'USAGE'
Usage:
  scripts/dev-build-image.sh [--target <name>] [--tag <tag>]

Builds Dockerfile.unified for a given agent/platform target and pushes to
localhost:5555 when the local registry is running.

Targets (common):
  agentcore   openab binary only (echo agent for gateway smoke tests)
  kiro        kiro-cli + openab
  claude      claude-agent-acp + openab
  codex       codex-acp + openab

Environment:
  IMAGE_NAME   Default: openab
  TAG          Default: <target>-dev
  TARGET       Default: agentcore
  REGISTRY     Default: localhost:5555
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) TARGET="${2:?}"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    --tag) TAG="${2:?}"; shift 2 ;;
    --tag=*) TAG="${1#*=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

command -v docker >/dev/null || die "missing docker"

if [[ -z "$TAG" ]]; then
  TAG="${TARGET}-dev"
fi

if [[ -z "$PLATFORM" ]]; then
  case "$(uname -m)" in
    arm64|aarch64) PLATFORM="linux/arm64" ;;
    x86_64|amd64) PLATFORM="linux/amd64" ;;
    *) die "unknown arch; set PLATFORM" ;;
  esac
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_TAG="${IMAGE_NAME}:${TAG}"
REMOTE_TAG="${REGISTRY}/${LOCAL_TAG}"

echo "building $LOCAL_TAG (platform=$PLATFORM, target=$TARGET)..."
docker build --platform "$PLATFORM" -f "$ROOT/Dockerfile.unified" --target "$TARGET" -t "$LOCAL_TAG" "$ROOT"

if curl -fsS "http://${REGISTRY}/v2/" >/dev/null 2>&1; then
  docker tag "$LOCAL_TAG" "$REMOTE_TAG"
  docker push "$REMOTE_TAG"
  echo "pushed $REMOTE_TAG"
  echo "image=$REMOTE_TAG"
else
  echo "local registry not reachable at $REGISTRY — tagged locally only: $LOCAL_TAG"
  echo "image=$LOCAL_TAG"
fi