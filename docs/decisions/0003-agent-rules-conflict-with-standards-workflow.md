---
id: "0003"
title: "Upstream agent rules conflict with the standards workflow"
status: accepted
date: 2026-10-26
deciders: [owner]
consulted: []
supersedes: []
superseded_by: null
tags: [standards, agents, fork]
links:
  - https://gitea.xetk.co.uk/xetk/zed/issues/63
---

# Upstream agent rules conflict with the standards workflow

## Context

Upstream ships agent instructions in `.rules` (`AGENTS.md`, `CLAUDE.md` and `GEMINI.md` are symlinks to it). Existing text was kept unchanged; only a marked section was appended. They conflict with the standards in these places:

- A rule to prepend a "remove this line to confirm you reviewed this PR" banner to `README.md` whenever source files change, never removable by an agent. Standards adoption says to add to `README.md`, not rewrite it.
- PR titles must be imperative with no conventional-commit prefixes; the standards require Conventional Commits and issue links.
- The PR body must end with a `Release Notes:` section; the standards PR body ends with the agent attribution line.
- Upstream says not to edit `.rules` inline (rules are added in dedicated commits after review). `AGENTS.md`, `CLAUDE.md` and `GEMINI.md` are all symlinks to `.rules`, so the standards section had to be appended there to reach any agent. It sits at the end, which may conflict when upstream edits the end of that file.
- Upstream's `.gitea/pull_request_template.md` has no standards checklist (see 0004).

## Options considered

- A. Keep both; the owner decides per case (current state, slow).
- B. Standards win in this fork's own changes; upstream rules apply to code taken from upstream. **Chosen.**
- C. Upstream rules win everywhere; drop the conflicting standards rules.

## Decision

Option B, chosen by the owner on 2026-10-26. For changes made only in this fork the standards workflow wins where it conflicts with upstream's agent rules; upstream's rules apply in full to code taken from upstream and to anything offered to the upstream project.

## Consequences

- The precedence paragraph in the fork section of `.rules` states this, and which PR title and body format to use for fork-only PRs.
- Upstream's text in `.rules` is unchanged; only the fork section differs from upstream, so syncing upstream stays easy.
- No removal date: this is a scope decision.
