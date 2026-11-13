# BT-App: Rust torrent client (engine + API + TUI, later Tauri GUI)

## Goal
Small, low-resource BitTorrent client. Cross-platform (Windows/Linux/macOS).
Engine is a library; TUI and GUI talk to it only through `EngineClient`.

## Workspace layout
- crates/proto: message types only, no logic, serde derives
- crates/engine: core logic (uses `rbit` for building blocks)
- crates/client: EngineClient trait; InProcess and Remote impls
- crates/daemon: headless binary exposing the API
- crates/tui: ratatui + crossterm front-end
- crates/platform: OS-specific code behind cfg

## Hard rules
- Never implement protocol details from memory. Read docs/beps/bep_XXXX.md
  first. If the note is missing or unclear, STOP and ask.
- No `unwrap()`/`expect()` in non-test code. Use `thiserror` errors.
- No `unsafe` without a comment explaining why and a test.
- All parsers of network/file input must handle malformed data without
  panicking, and get a fuzz target.
- TUI/GUI code must not import engine internals; use crates/client only.
- Never block the async runtime (no blocking disk I/O on tokio worker threads).
- Add dependencies only when necessary; justify in the PR. Check size impact.
- Do not hardcode paths; use `directories`/platform crate.

## Workflow
1. Read this file and the relevant skill (`skill` tool) before coding.
2. Write a failing test first, then implement.
3. Before finishing, run:
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
4. Keep diffs small (about 200 lines). One concern per commit.
5. Update docs/BEP_STATUS.md when a BEP feature changes.

## Commits and branches
Conventional Commits (feat:, fix:, test:, docs:, refactor:, chore:).
Branches: feat/<area>-<topic>, fix/<topic>.

HUMAN APPROVAL REQUIRED: never run git commit, create or switch branches,
merge, rebase, reset, tag, or push on your own initiative. Before each one,
tell the human the exact command, branch name or commit message, and what
will be included (git status --short). Wait for approval, then run it so the
permission prompt can appear. One commit per approval. If a command is
denied, do not look for a workaround (no scripts, aliases, or other tools).

## Skills available
Load with the skill tool: rust-conventions, bep-implementation,
engine-api-design, tui-ratatui, protocol-testing, git-workflow.

## Out of scope for v0.1
GUI, uTP, encryption, RSS, scheduler, v2/hybrid torrents.

## Approval rules (mandatory)
- Never run `git commit`, create branches, or change dependencies without
  my explicit approval in the chat.
- Before each commit: show `git status`, `git diff --stat`, and the proposed
  commit message, then STOP and wait for "approved".
- Never edit AGENTS.md, opencode.json, .opencode/**, or CI files unless I
  asked for that exact change.
- Never push. I do that myself.
- If a plan was requested, output the plan and wait. Do not start work.