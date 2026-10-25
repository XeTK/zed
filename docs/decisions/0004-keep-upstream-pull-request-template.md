---
id: "0004"
title: "Keep upstream's pull request template instead of the standards one"
status: proposed
date: 2026-10-25
deciders: [owner]
consulted: []
supersedes: []
superseded_by: null
tags: [standards, templates, fork]
links:
  - https://gitea.xetk.co.uk/xetk/zed/issues/63
---

# Keep upstream's pull request template instead of the standards one

## Context

`.gitea/pull_request_template.md` already exists (Zed's checklist and Release Notes). Adding the standards `PULL_REQUEST_TEMPLATE.md` next to it would create two templates for one repo, and Gitea would pick one unpredictably. The issue templates were added (none existed).

## Decision

Keep the existing template. Merge the standards checklist into it, or replace it, once 0003 is decided. Target date: 2026-11-30.

## Consequences

PRs here lack the standards checklist until then.
