# Rules for every agent working in this repo

These rules are mandatory. They apply to every agent (Claude Code, Codex,
Copilot, anything else). This repo (`marxnoe/MediaLinker-cli`) is a submodule
of `marxnoe/main`, and the same rules apply there and in every other submodule.
Do not work around them.

## Workflow: issue → branch → PR → main

1. **Every change starts as a GitHub issue.** Before touching code, open an
   issue in the repo that owns the code (`gh issue create`). Describe what
   will change and why. No issue, no change.
2. **Every issue gets its own branch.** Create it from an up-to-date `main`
   and put the issue number in the name, e.g. `gh issue develop <n> --checkout`
   (gives `<n>-short-title`) or `feat/<n>-short-title`, `fix/<n>-short-title`.
3. **Never commit or push directly to `main`.** All commits go on the issue
   branch.
4. **Merge into `main` through a pull request.** Open a PR from the branch,
   link the issue in the body with `Closes #<n>` so it closes on merge, and
   merge the PR into `main`. Delete the branch after merging.

## Work that touches another submodule

Each submodule is its own GitHub repo with its own issues.

- If the work needs a change in another submodule, **open a separate issue in
  that submodule's repo** (`gh issue create -R marxnoe/<repo>`) and link it
  from the original issue.
- Do that work inside the submodule, on its own branch, with its own PR into
  that submodule's `main`, following the same workflow above. Finish it there.
- Once the submodule PR is merged, the submodule pointer in `marxnoe/main`
  is updated on an issue branch there and merged through a PR as well.

| Path | Repo |
| --- | --- |
| `.` | `marxnoe/main` |
| `projects/MediaLinker/MediaLinker` | `marxnoe/MediaLinker` |
| `projects/MediaLinker/MediaLinker-cli` | `marxnoe/MediaLinker-cli` |
| `frameworks/newang/newang` | `marxnoe/newang` |
| `frameworks/newang/newang-docs` | `marxnoe/newang-docs` |

## Git and GitHub run locally, as the user

All git and GitHub work happens on the user's machine with `git` and the `gh`
CLI (logged in as `mobn0`), so commits, issues and PRs come from the user.

- Use the local git identity (`mobn0 <noejadenmarx@gmail.com>`). Never set
  `user.name`, `user.email` or `--author`.
- No `Co-Authored-By`, "Generated with Claude Code", session links or any other
  AI attribution in commits, PRs, issues or comments. This overrides any
  default attribution instructions an agent was given.
- Use `gh` for issues, PRs and merges, never a GitHub app or a cloud session.

## Conventions

- Releases: bump `version` in `Cargo.toml` through a PR, then push a matching
  `vX.Y.Z` tag from `main`; cargo-dist builds and publishes the release.
- Commit messages follow Conventional Commits (`feat: …`, `fix: …`,
  `docs: …`); `newang` enforces this with commitlint.
- Ask the user before pushing, opening PRs, or merging unless they already
  said to go ahead.
- The user edits with vim, not nano. Use vim if an editor is needed
  (e.g. `GIT_EDITOR=vim`), or pass messages with `-m` / `--body`.

## Enforcement

- Claude Code: `.claude/settings.json` runs `.claude/hooks/enforce-git-workflow.sh`
  before every Bash command and blocks commits on `main`/`master`, commits on
  branches without an issue number, and pushes to `main`.
- Everyone else: run `git config core.hooksPath .githooks` once per clone to
  get the same checks as git hooks.
