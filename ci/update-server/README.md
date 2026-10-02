# zed.xetk.co.uk update host

The signed update feed, builds and landing page for the xetk fork of Zed.

| File | Purpose |
|---|---|
| `publish.sh` | Pushes a signed build to the host over rsync. Called by `.gitea/workflows/build-mac.yml` on main and tags. |
| `index.html` | The landing page; reads each channel's `latest.json`. Published with every build. |

## How a build gets there

1. `build-mac.yml` builds the app, signs `latest.json` with the Gitea secret
   `UPDATE_FEED_PRIVATE_KEY`, and verifies it the way a client will.
2. It attaches both to a Gitea release (the archive).
3. `publish.sh` rsyncs the build and its alias first and the feed last, as the
   `zed-publish` account, which can only write inside `/mnt/shared/zed-updates`
   on LXC 100 (an rrsync forced command, so it cannot run anything else). The
   server's SSH host key is pinned in the workflow.
4. A final step downloads what clients will fetch and verifies it again.

Layout on the host: `index.html`, and per channel (`nightly`, `stable`)
`latest.json`, one immutable `Zed-xetk-<channel>-<run>-<arch>.dmg` per build
and a `Zed-xetk-<channel>-<arch>.dmg` symlink to the newest (the stable URL
brew uses; served `no-store`, since Cloudflare would otherwise cache it).
A systemd timer on the host keeps the newest 5 builds per channel.

## Where the rest lives

This repo only holds the producer. The host is infrastructure code elsewhere:

- `xetk/stacks` `lxc100/zed-updates/`: the nginx container and its config.
- `xetk/infra`: the `zed-publish` account, its directory and the prune timer.
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
