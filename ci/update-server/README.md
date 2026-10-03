# zed.xetk.co.uk update host

The signed update feed, builds and landing page for the xetk fork of Zed.

| File | Purpose |
|---|---|
| `publish.sh` | Pushes a signed build to the host over rsync. Called by `.gitea/workflows/build-mac.yml` on main and tags. |
| `index.html` | The landing page; reads each platform's `latest.json` and lists the platforms that have one, the visitor's first. Published with every build. |
| `features.html`, `features.json` | The page describing what the fork adds and how to enable each feature. The page only renders; **to document a feature, add an entry to `features.json`** (see below). Published with every build. |

## How a build gets there

1. `build-mac.yml` builds the app, signs `latest.json` with the Gitea secret
   `UPDATE_FEED_PRIVATE_KEY`, and verifies it the way a client will.
2. It attaches both to a Gitea release (the archive).
3. `publish.sh <channel> <os>-<arch> <build file> latest.json` rsyncs the build and its alias first and the feed last, as the
   `zed-publish` account, which can only write inside `/mnt/shared/zed-updates`
   on LXC 100 (an rrsync forced command, so it cannot run anything else). The
   server's SSH host key is pinned in the workflow.
4. A final step downloads what clients will fetch and verifies it again.

Layout on the host:

```
index.html
features.html
features.json
<channel>/latest.json                                 legacy, see below
<channel>/<os>-<arch>/latest.json                     the signed feed
<channel>/<os>-<arch>/Zed-xetk-<channel>-<run>-<arch>.<ext>   one immutable file per build
<channel>/<os>-<arch>/Zed-xetk-<channel>-<os>-<arch>.<ext>    symlink to the newest build
```

`<channel>` is `nightly` or `stable`; `<os>-<arch>` is the platform as the app
and the feed spell it (`std::env::consts::OS` and `ARCH`, for example
`macos-aarch64`, `linux-x86_64`, `windows-x86_64`), so the client builds the
feed URL as `{base}/{channel}/{os}-{arch}/latest.json` with no table to keep in
step. The alias is what brew and the landing page link to; it is served
`no-store`, since Cloudflare would otherwise cache it. A systemd timer on the
host keeps the newest 5 builds per channel and platform.

Adding a platform means a build job that produces the file, signs a feed with
`update_feed sign --os <os> --arch <arch> --url <.../<channel>/<os>-<arch>/<file>>`,
and calls `publish.sh` with that platform and file. Nothing on the host is per
platform except the file extensions nginx knows about (see below).

### Old feed URL

Before feeds were per platform, the only build (macOS on Apple silicon) read
`<channel>/latest.json`. `publish.sh` still writes a copy of the
`macos-aarch64` feed there, byte for byte: it is signed and names the new
download URL, so a copy built against the old URL verifies it, downloads from
the new place and carries on. Drop the copy (the `legacy_feed` lines in
`publish.sh`, and the matching check in `build-mac.yml`) once no installed
build uses the old URL.

## Where the rest lives

This repo only holds the producer. The host is infrastructure code elsewhere:

- `xetk/stacks` `lxc100/zed-updates/`: the nginx container and its config,
  whose location blocks name the platform directories and the file extensions
  (`.dmg`, `.tar.gz`, `.exe`, `.zip`).
- `xetk/infra`: the `zed-publish` account, its directory and the prune timer,
  which walks `<channel>/<platform>/`.
- `xetk/homelab`: the docs, including the Caddy route, DNS and Cloudflare
  tunnel steps, which are done by hand per its adding-a-service playbook.

## Keys

- `ZED_UPDATE_PUBLIC_KEY` (in `build-mac.yml`) is compiled into the app.
- `UPDATE_FEED_PRIVATE_KEY` (Gitea secret) signs feeds. **Keep an off-box
  backup.** If it is lost, installed copies can no longer verify new feeds and
  have to be reinstalled by hand from a build carrying a new public key.
- `ZED_PUBLISH_SSH_KEY` (Gitea secret) is the `zed-publish` account's key. It
  is replaceable: change the key in `xetk/infra`'s `guest_restricted_users`
  and the secret together.

## Documenting a feature

Every user-facing feature adds one entry to `features.json`, in the same PR:

```json
{
  "section": "agent",
  "title": "Short name",
  "summary": "One line, optional.",
  "details": ["How it works and how to turn it on. `code` and **bold** are supported."],
  "example": "{ \"agent\": { \"some_setting\": true } }",
  "settings": ["some_setting"],
  "pr": 123
}
```

`section` must be one of the ids in `sections`. `settings` lists the keys the
entry mentions; `test_features.py` checks each one exists in
`assets/settings/default.json`, so a renamed or mistyped setting fails CI.
`.gitea/workflows/features-check.yml` runs it on pull requests that touch this
directory or the default settings.
