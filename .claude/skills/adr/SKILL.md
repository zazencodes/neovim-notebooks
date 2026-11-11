---
name: adr
description: Read and write Architecture Decision Records (ADRs) in docs/adr/. Use when the user types /adr, asks to write an ADR or record a decision, when a real decision is made during work, and before changing an area of a project that has ADRs.
---

# ADR

An ADR records a standing decision so the next session neither undoes it nor argues it again. The code is the source of truth for how things work; an ADR keeps only what was decided, why, and what was ruled out.

## Where they live

- `docs/adr/`, one file per decision, named `YYYY-MM-DD-<topic>.md`. The topic is a short claim: `the-clip-title-names-the-clip`, `postgres-full-text-search`.
- A repo of independent projects keeps them per project in `<project>/docs/adr/`. Follow the repo's `AGENTS.md`.
- The project's `AGENTS.md` indexes them under `## Decisions`: one line per ADR, a short summary and the path.
- Create `docs/adr/` when writing the first one.

## Before changing an area

Read the index in `AGENTS.md`, open every ADR that touches the area, and work within them. If the task conflicts with one, stop and tell the user which ADR and how before writing code. Never work around an ADR quietly.

## When to write one

Write one whenever a real decision is made: a choice between viable approaches, a rejected alternative, or a constraint the next session would otherwise undo. Write it in the same change as the code. Skip choices with one obvious answer.

Invoked as `/adr` with nothing else, record the decision just made in the conversation. If none is clear, ask which one.

## Format

```md
# The decision, stated as a short claim

## Decision

What was decided, in a few sentences or bullets.

## Why

The reasons: the needs and constraints that settled it.

## Rejected

- **The alternative.** Why it lost.
```

Say what must not be re-litigated in Decision or Rejected. Keep it to one screen.

## Rules

- Stay high-level. Never restate function names, schemas, config values, geometry or anything else a refactor would change. If a refactor could make a sentence false while the decision still stands, cut the sentence.
- Supersede an ADR by editing or deleting it, never by writing a new one that contradicts it.
- Add, update or remove its line in the `AGENTS.md` index in the same change.
