# zed.xetk.co.uk update host

Static hosting for the fork's signed update feed, builds and landing page. The
build workflow (`.gitea/workflows/build-mac.yml`) signs each build and attaches
it, with `latest.json`, to a Gitea release. `publish-update.sh` pulls that
release onto this host; nginx serves the result. The host never needs write
access to anything, and the script refuses a build whose signature or hash
does not verify.

| File | Purpose |
|---|---|
| `publish-update.sh` | Pulls a channel's newest release from Gitea, verifies it, and lays it out under the served directory. |
| `nginx.conf` | Serves that directory (feed uncached, per-build files immutable). |
| `docker-compose.yml` | Runs nginx on the reverse proxy's `proxy` network, no published ports. |
| `index.html` | Landing page; reads each channel's `latest.json`. |

## Deploying (homelab)

1. Create `/mnt/shared/zed-updates` and put `nginx.conf` and
   `docker-compose.yml` in a stack directory; `docker compose up -d`.
2. Reverse proxy: route `zed.xetk.co.uk` to `zed-updates:80`, the same way the
   F-Droid repo is routed.
3. Cloudflare: add the tunnel ingress rule and DNS record, with no Access
   application (the app and brew fetch it unauthenticated, as with F-Droid).
4. On a host that can reach Gitea and write the directory, build the verifier
   once (`cargo build --release -p update_feed`) and run, for both channels,
   from cron or a systemd timer:

   ```
   GITEA_TOKEN=<read-only token> UPDATE_FEED_BIN=/path/to/update_feed \
     ./publish-update.sh nightly
   ```

   `DEST`, `GITEA_URL`, `REPO`, `KEEP` and `UPDATE_PUBLIC_KEY` can be
   overridden; the defaults match this setup.

## Keys

The public key is compiled into the app (`ZED_UPDATE_PUBLIC_KEY` in
`build-mac.yml`) and is the default in `publish-update.sh`. The private key is
the Gitea secret `UPDATE_FEED_PRIVATE_KEY`; keep an off-box backup. If it is
lost, copies already installed can no longer verify new feeds and have to be
reinstalled by hand with a build that carries a new public key.
