#!/bin/sh

set -eu

usage() {
  echo "Usage: github_publish_preflight.sh --repo owner/name [--label label-name]" >&2
}

repo=""
label=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo)
      [ "$#" -ge 2 ] || {
        usage
        exit 1
      }
      repo="$2"
      shift 2
      ;;
    --label)
      [ "$#" -ge 2 ] || {
        usage
        exit 1
      }
      label="$2"
      shift 2
      ;;
    *)
      usage
      exit 1
      ;;
  esac
done

[ -n "$repo" ] || {
  usage
  exit 1
}

echo "github_publish_preflight=status=starting repo=$repo"
gh auth status -h github.com >/dev/null
gh repo view "$repo" --json name,viewerPermission,defaultBranchRef >/dev/null
gh pr list --repo "$repo" --state all --limit 1 >/dev/null

if [ -n "$label" ]; then
  gh label list --repo "$repo" --limit 200 | awk -F '\t' -v wanted="$label" '
    $1 == wanted { found = 1 }
    END { exit found ? 0 : 1 }
  '
fi

echo "github_publish_preflight=status=ok repo=$repo${label:+ label=$label}"
