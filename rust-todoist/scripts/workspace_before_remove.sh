#!/bin/sh

set -eu

DEFAULT_COMMENT='Closing because the Todoist task for the current branch entered a terminal state without merge.'

repo_arg=""
branch_arg=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo)
      repo_arg="${2:-}"
      shift 2
      ;;
    --branch)
      branch_arg="${2:-}"
      shift 2
      ;;
    --help|-h)
      echo "Usage: workspace_before_remove.sh [--repo owner/name] [--branch branch-name]"
      exit 0
      ;;
    *)
      echo "Ignoring unsupported argument: $1" >&2
      shift
      ;;
  esac
done

if ! command -v gh >/dev/null 2>&1; then
  exit 0
fi

if ! gh auth status >/dev/null 2>&1; then
  exit 0
fi

branch="${branch_arg}"
if [ -z "$branch" ]; then
  branch="$(git branch --show-current 2>/dev/null || true)"
fi

if [ -z "$branch" ]; then
  exit 0
fi

repo="${repo_arg}"
if [ -z "$repo" ]; then
  remote_url="$(git config --get remote.origin.url 2>/dev/null || true)"
  case "$remote_url" in
    git@github.com:*.git)
      repo="${remote_url#git@github.com:}"
      repo="${repo%.git}"
      ;;
    https://github.com/*)
      repo="${remote_url#https://github.com/}"
      repo="${repo%.git}"
      ;;
  esac
fi

if [ -z "$repo" ]; then
  exit 0
fi

prs="$(gh pr list --repo "$repo" --head "$branch" --state open --json number --jq '.[].number' 2>/dev/null || true)"

if [ -z "$prs" ]; then
  exit 0
fi

echo "$prs" | while IFS= read -r pr_number; do
  if [ -z "$pr_number" ]; then
    continue
  fi

  if gh pr close "$pr_number" --repo "$repo" --comment "$DEFAULT_COMMENT" >/dev/null 2>&1; then
    printf 'Closed PR #%s for branch %s\n' "$pr_number" "$branch"
  else
    printf 'Failed to close PR #%s for branch %s\n' "$pr_number" "$branch" >&2
  fi
done
