#!/usr/bin/env bash
# Ship the current (persistent) dev branch to its base and re-sync it.
#
# Flow: push -> open/reuse PR -> wait for CI -> merge (merge commit) -> fast-forward
# the dev branch onto the merge commit -> push. The dev branch is NEVER deleted
# (it is long-lived); after the merge it equals the base again, so work continues
# on it. Merge strategy is a merge commit (matches this repo's PR convention) so
# the persistent branch fast-forwards cleanly with no orphaned/duplicated commits.
#
# Usage: scripts/dev-merge-sync.sh [-y] [-b base] [-t "PR title"]
#   -y         skip the pre-merge confirmation
#   -b base    base branch to merge into (default: master)
#   -t title   PR title when creating a new PR (default: latest commit subject)
#
# Requires: authenticated `gh`, a clean working tree (tracked changes), and being
# on the dev branch (not the base). Untracked files are ignored, so this script
# living uncommitted under scripts/ does not block itself.
set -euo pipefail

BASE="master"
ASSUME_YES=0
TITLE=""
while getopts "yb:t:" opt; do
  case "$opt" in
    y) ASSUME_YES=1 ;;
    b) BASE="$OPTARG" ;;
    t) TITLE="$OPTARG" ;;
    *) echo "usage: $0 [-y] [-b base] [-t title]" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }

command -v gh >/dev/null || die "gh CLI not found"
DEV="$(git rev-parse --abbrev-ref HEAD)"
[ "$DEV" != "$BASE" ] || die "on '$BASE' — run from your dev branch, not the base"
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "uncommitted changes — commit or stash first"

echo "==> dev branch: $DEV  ->  base: $BASE"
git fetch origin --quiet

AHEAD="$(git rev-list --count "origin/$BASE..$DEV")"
[ "$AHEAD" -gt 0 ] || die "'$DEV' has no commits over origin/$BASE — nothing to ship"
[ "$(git rev-list --count "$DEV..origin/$BASE")" -eq 0 ] || die "'$DEV' is behind origin/$BASE — rebase/merge it first"
echo "==> $AHEAD commit(s) to ship"

git push -u origin "$DEV"

# Reuse an open PR for this head->base, else create one.
PR="$(gh pr list --head "$DEV" --base "$BASE" --state open --json number -q '.[0].number' || true)"
if [ -z "$PR" ]; then
  if [ -n "$TITLE" ]; then
    BODY="$(git log "origin/$BASE..$DEV" --format='- %s')"
    gh pr create --base "$BASE" --head "$DEV" --title "$TITLE" --body "$BODY"
  else
    gh pr create --base "$BASE" --head "$DEV" --fill
  fi
  PR="$(gh pr list --head "$DEV" --base "$BASE" --state open --json number -q '.[0].number' || true)"
  echo "==> created PR #$PR"
else
  echo "==> reusing PR #$PR"
fi
[ -n "$PR" ] || die "could not resolve a PR number"

echo "==> waiting for checks on PR #$PR ..."
gh pr checks "$PR" --watch --fail-fast || die "checks failed/cancelled on PR #$PR — not merging"

if [ "$ASSUME_YES" -ne 1 ]; then
  printf "==> merge PR #%s into %s and fast-forward %s? [y/N] " "$PR" "$BASE" "$DEV"
  read -r ans
  case "$ans" in y | Y | yes | YES) ;; *) die "aborted" ;; esac
fi

# Merge commit; keep the branch (it is the persistent dev branch).
gh pr merge "$PR" --merge

# Re-sync: ff the dev branch onto the merged base, push (recreates it if the merge
# auto-deleted the remote head), and point the local base ref at the merge commit.
git fetch origin --quiet
git merge --ff-only "origin/$BASE"
git push origin "$DEV"
git branch -f "$BASE" "origin/$BASE"

echo "==> done: $DEV == $BASE == $(git rev-parse --short "origin/$BASE")"
