---
description: Draft the pull request text for the current branch
---
Prepare the pull request for the current branch against main. Do not run
any git write command and do not push. Read git log main..HEAD and
git diff main...HEAD --stat. Output in chat only: a Conventional Commit
style title and a body with Summary, Commits, Security notes (if any),
How to verify (exact commands and expected test count), Not included /
follow-ups, and the git-workflow checklist filled in honestly, including
dependency and release binary-size impact. List anything uncertain.