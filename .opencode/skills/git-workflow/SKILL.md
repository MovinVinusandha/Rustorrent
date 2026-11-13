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