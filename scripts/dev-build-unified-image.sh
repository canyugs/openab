#!/usr/bin/env bash
# Back-compat wrapper — builds agentcore (unified adapters) image.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG="${TAG:-unified-dev}"
exec "${ROOT}/scripts/dev-build-image.sh" --target agentcore --tag "$TAG" "$@"