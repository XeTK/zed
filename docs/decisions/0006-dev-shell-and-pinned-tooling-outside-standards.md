---
id: "0006"
title: "Dev shell, direnv and some CI tooling are outside the standards"
status: proposed
date: 2026-10-25
deciders: [owner]
consulted: []
supersedes: []
superseded_by: null
tags: [nix, tooling, gap]
links:
  - https://gitea.xetk.co.uk/xetk/zed/issues/63
---

# Dev shell, direnv and some CI tooling are outside the standards

## Context

`flake.nix` is upstream's (package build), with no `flake.lock`, no `.envrc` and no `devShells` in the root flake. Rust is pinned by `rust-toolchain.toml`. The macOS CI jobs install tools with the host's package manager and rustup, not `nix develop --command`, because they run on a native macOS runner.

## Decision

Accept for now. Add a committed `flake.lock`, a dev shell and `.envrc` for fork work, and move CI steps into `nix develop` where a runner can do it. The macOS build itself may stay native; if so record it as its own decision. Target date: 2027-06-30.

## Consequences

Local and CI tool versions can drift until then.
