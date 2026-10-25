---
id: "0001"
title: "Adopt engineering standards in this repo"
status: accepted
date: 2026-10-25
deciders: [owner]
consulted: []
supersedes: []
superseded_by: null
tags: [standards, process]
links:
  - https://gitea.xetk.co.uk/xetk/zed/issues/63
  - https://gitea.xetk.co.uk/xetk/engineering-standards
---

# Adopt engineering standards in this repo

## Context

The owner asked on 2026-10-25 for [engineering-standards](https://gitea.xetk.co.uk/xetk/engineering-standards) v1 to be applied to this repo. It is a fork of the Zed editor (`zed-industries/zed`) with fork-only features and Gitea CI added.

## Decision

Follow engineering-standards v1 with the `stack-defaults` profile. The `homelab` profile is not adopted: the main deliverable is an editor build, and its CI runs mostly on a macOS runner rather than the NixOS runners. Records live in `docs/decisions/`; that path does not exist upstream, so merges from upstream do not collide with it.

## Consequences

- Changes made in this fork are held to the standards; upstream code is not (see 0002).
- Gaps found at adoption are proposed as decisions 0002 to 0006, each with a target date.
