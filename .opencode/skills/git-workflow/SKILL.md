---
name: git-workflow
description: Use before committing, branching, or preparing a pull request.
---

## Branches
- Never commit to main. Branch names: feat/<area>-<topic>, fix/<topic>,
  docs/<topic>, chore/<topic>, test/<topic>.
- Short-lived branches, one concern each, squash-merge into main.

## Commits
Conventional Commits: feat, fix, test, docs, refactor, perf, chore, ci.
Scope = crate or area, e.g. `feat(engine): add piece picker`.
Small commits, each building and passing tests.

## PR checklist
- [ ] Tests added or updated
- [ ] fmt, clippy -D warnings, tests pass
- [ ] docs/BEP_STATUS.md updated if protocol behavior changed
- [ ] docs/beps/ note added or corrected
- [ ] No new unwrap/expect, no new heavy dependency without justification
- [ ] Binary size checked if dependencies changed

## Never
Force-push main, commit secrets, commit target/ or downloaded data,
bypass CI.

## Pull requests
- One PR per roadmap item or split step; open it when the branch is
  review-ready: local checks green and the reviewer agent's blockers and
  majors fixed. Never stack new work on an unmerged branch.
- The agent drafts the PR text but never pushes. The human pushes, opens
  the PR, waits for CI on all three OSes, and merges with --ff-only.
- Keep commits separate; do not squash.