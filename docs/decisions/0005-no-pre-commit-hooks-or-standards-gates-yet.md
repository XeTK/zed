---
id: "0005"
title: "No pre-commit, coverage gate, secrets scan or Gherkin yet"
status: proposed
date: 2026-10-25
deciders: [owner]
consulted: []
supersedes: []
superseded_by: null
tags: [ci, testing, gap]
links:
  - https://gitea.xetk.co.uk/xetk/zed/issues/63
---

# No pre-commit, coverage gate, secrets scan or Gherkin yet

## Context

There is no `.pre-commit-config.yaml`, no `gitleaks` run, no coverage gate and no `features/`. CI is `.gitea/workflows/test-mac.yml` (full workspace tests on a macOS runner) and a few release workflows. The stack-defaults profile has no Rust row, so the Rust tools (`cargo fmt`, `./script/clippy`, coverage tool) are not yet chosen.

## Decision

Accept the gap for now. Add the Rust row to the profile (or a decision here), then a pre-commit config with `gitleaks`, `cargo fmt --check`, markdownlint and yamllint, run in CI on a Linux runner, scoped to fork-owned paths (0002). A 100 % coverage gate applies to fork-only crates only, not to upstream code. Target date: 2027-03-31.

## Consequences

Until then the standards checks are done by hand in reviews.
