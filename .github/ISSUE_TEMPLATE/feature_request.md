---
name: Feature request
about: Propose a new pq capability — a DSL stage, TUI affordance, output format, or workflow.
title: "vX.Y: <one-line summary of the capability>"
labels: enhancement
assignees: ""
---

<!--
  This template mirrors how pq features have historically been specced
  (see the v0.14 issues #2/#3/#4). The goal is that anyone — including
  future-you — can read the issue and start implementing without a
  conversation. Delete sections that genuinely don't apply, but prefer
  filling them in: the "Proposed approach" + "Acceptance criteria"
  pairing is what makes these tractable in a weekend.
-->

## Background

<!--
  What's the real-world pain? Lead with the workflow that hurts today,
  ideally a concrete shell snippet showing the current workaround and
  where it falls apart. The best pq issues start from "I do X dozens of
  times a day and Y is the friction."
-->

```bash
# current workaround, and where it breaks
```

## Proposed approach

<!--
  How would you build it? For DSL changes, show the new syntax and a
  before/after. For new subcommands, sketch the `Cmd` enum variant /
  clap args. For TUI work, name the key binding and the panel it touches.
  Point at the file(s) you expect to change (src/main.rs, src/tui.rs,
  src/output.rs, …). Doesn't have to be final — it has to be a starting
  point someone can argue with.
-->

```rust
// sketch of the new DSL stage / Cmd variant / function signature
```

## Acceptance criteria

<!-- Checklist of what "done" means. Be specific enough to write tests against. -->

- [ ]
- [ ]
- [ ] Unit + integration tests cover the happy path and at least one edge case
- [ ] `README.md` "What's done" updated; item removed from "What's coming"
- [ ] `doc/reference.md` and/or `doc/tutorial.md` updated if user-facing

## Out of scope / non-goals

<!--
  What this issue deliberately does NOT do. Keeps the PR small and stops
  scope creep. Link follow-up ideas instead of absorbing them here.
-->

## Notes / open questions

<!--
  DuckDB quirks, prior art in jq, parsing ambiguities (the " where "
  split is naive — does this interact?), perf concerns on big files, etc.
-->
