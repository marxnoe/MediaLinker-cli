#!/usr/bin/env bash
# PreToolUse hook: enforce the issue -> branch -> PR workflow from AGENTS.md.
# Exit 2 blocks the Bash command and shows stderr to the agent.
input=$(cat)
cmd=$(jq -r '.tool_input.command // empty' <<<"$input")
cwd=$(jq -r '.cwd // empty' <<<"$input")
[ -z "$cmd" ] && exit 0

# Only care about git commit / git push.
grep -Eq '(^|[;&|[:space:]])git([[:space:]]+-C[[:space:]]+[^[:space:]]+)*[[:space:]]+(commit|push)([[:space:]]|$)' <<<"$cmd" || exit 0

# Work out which repo the command runs in: honour a leading `cd X` and `git -C X`.
dir=${cwd:-$PWD}
cd_target=$(grep -Eo '(^|[;&|][[:space:]]*)cd[[:space:]]+[^;&|[:space:]]+' <<<"$cmd" | tail -1 | awk '{print $NF}')
[ -n "$cd_target" ] && dir=$(cd "$dir" 2>/dev/null && cd "${cd_target/#\~/$HOME}" 2>/dev/null && pwd || echo "$dir")
c_target=$(grep -Eo 'git[[:space:]]+-C[[:space:]]+[^[:space:]]+' <<<"$cmd" | tail -1 | awk '{print $NF}')
[ -n "$c_target" ] && dir=$(cd "$dir" 2>/dev/null && cd "${c_target/#\~/$HOME}" 2>/dev/null && pwd || echo "$dir")

branch=$(git -C "$dir" symbolic-ref --short HEAD 2>/dev/null)

deny() {
  echo "Blocked by AGENTS.md workflow: $1" >&2
  echo "Open a GitHub issue, work on a branch named with its number (e.g. 'gh issue develop <n> --checkout'), and merge into main via a PR." >&2
  exit 2
}

if grep -Eq 'git([[:space:]]+-C[[:space:]]+[^[:space:]]+)*[[:space:]]+commit' <<<"$cmd"; then
  case "$branch" in
    main|master) deny "no direct commits on '$branch'." ;;
  esac
  if [ -n "$branch" ] && ! grep -Eq '(^|/)(issue-)?[0-9]+-' <<<"$branch"; then
    deny "branch '$branch' has no issue number in its name."
  fi
fi

if grep -Eq 'git([[:space:]]+-C[[:space:]]+[^[:space:]]+)*[[:space:]]+push' <<<"$cmd"; then
  if grep -Eq 'git[^;&|]*push[^;&|]*([[:space:]]|:)(main|master)([[:space:]]|$|;|&|\|)' <<<"$cmd"; then
    deny "no direct pushes to main."
  fi
  case "$branch" in
    main|master)
      grep -Eq 'git[^;&|]*push[^;&|]*[[:space:]][^-[:space:]][^[:space:]]*[[:space:]]+[^-[:space:]]' <<<"$cmd" \
        || deny "you are on '$branch'; a plain push would push to main." ;;
  esac
fi
exit 0
