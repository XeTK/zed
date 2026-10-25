---
id: "0002"
title: "Upstream code is not held to the standards"
status: proposed
date: 2026-10-25
deciders: [owner]
consulted: []
supersedes: []
superseded_by: null
tags: [standards, fork]
links:
  - https://gitea.xetk.co.uk/xetk/zed/issues/63
---

# Upstream code is not held to the standards

## Context

Almost all of this repo is upstream Zed: hundreds of crates, upstream docs, scripts and GitHub CI. Bringing it to the standards (100 % coverage, Gherkin, layout, runbooks) would diverge from upstream and make syncing upstream very hard (see the open upstream-sync conflict issue).

## Decision

Only changes made in this fork are held to the standards: fork-only crates and features (for example thread control in `agent_ui` and `cli`), `.gitea/`, `.rules` additions and `docs/decisions/`. Upstream files stay as they are. Target removal date: none; review 2027-10-25, or earlier if the fork stops tracking upstream.

## Consequences

- Reviews of upstream merges check only that fork changes did not regress.
- Standards checks must be scoped to fork-owned paths.
