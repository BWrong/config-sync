## Agent skills

### Issue tracker

Specs and tickets live as GitHub issues. Publishing one, reading one, or wiring blocking edges: `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical triage roles, default label strings. Role → label string: `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout: `GLOSSARY.md` at the root, decisions in `docs/adr/`. Vocabulary and ADR rules: `docs/agents/domain.md`.

### Product design

`DESIGN.md` carries the behaviour rules every implementation obeys — the six inventory states, two-phase apply, the push hard-block. Read it before writing code for a ticket, when changing a state rule, or when writing UI copy.
