---
name: bep-implementation
description: Use when implementing or changing any BitTorrent protocol feature (BEP). Defines the checklist, file notes, and test requirements.
---

## Steps
1. Read docs/beps/bep_<NNNN>.md. If absent, create it from the spec
   (bittorrent.org/beps) in your own words: message formats, byte layouts,
   edge cases, required behavior.
2. Check what `rbit` already provides before writing code. Prefer reusing it
   unless it is missing, buggy, or hurts size.
3. Write tests from the spec: encode/decode round-trips, malformed input,
   boundary values.
4. Implement in the matching engine module.
5. Add or extend a fuzz target if you parse untrusted bytes.
6. Update docs/BEP_STATUS.md (status, module, test names).
7. Run fmt, clippy, tests.

## Rules
- Private torrents (BEP 27): never use DHT/PEX/LSD for them.
- Never trust lengths from the wire; cap allocations.
- Do not invent fields. If the spec is ambiguous, stop and ask.