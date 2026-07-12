#!/usr/bin/env bash
# Back-compat wrapper — delegates to dev-up-unified.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Map legacy --namespace / --delete namespace wipe to profile delete.
DELETE=0
ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --delete)
      DELETE=1
      ARGS+=("$1")
      shift
      ;;
    --namespace)
      export OAB_KUBE_NAMESPACE="${2:?}"
      shift 2
      ;;
    --namespace=*)
      export OAB_KUBE_NAMESPACE="${1#*=}"
      shift
      ;;
    *)
      ARGS+=("$1")
      shift
      ;;
  esac
done

if [[ "$DELETE" == "1" && ${#ARGS[@]} -eq 1 ]]; then
  kubectl delete namespace "${OAB_KUBE_NAMESPACE:-openab-local}" --ignore-not-found 2>/dev/null || true
fi

exec "${ROOT}/scripts/dev-up-unified.sh" "${ARGS[@]}"