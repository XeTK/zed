#!/usr/bin/env bash
# Publishes the newest signed build of a channel from its Gitea release into
# the directory nginx serves. It pulls, so the public host never needs write
# access to anything, and it refuses anything whose signature or hash does not
# verify. Safe to run repeatedly (e.g. from cron or a systemd timer).
#
#   GITEA_TOKEN=... ./publish-update.sh nightly
#
# Needs: curl, python3, and the `update_feed` binary
# (`cargo build --release -p update_feed`, then put it on PATH or set
# UPDATE_FEED_BIN).
set -euo pipefail

channel="${1:?usage: publish-update.sh <nightly|stable>}"
case "${channel}" in
  nightly | stable) ;;
  *) echo "unknown channel: ${channel}" >&2; exit 1 ;;
esac

: "${GITEA_TOKEN:?set GITEA_TOKEN to a token that can read the releases of the repo}"
GITEA_URL="${GITEA_URL:-https://gitea.xetk.co.uk}"
REPO="${REPO:-xetk/zed}"
DEST="${DEST:-/mnt/shared/zed-updates}"
PUBLIC_KEY="${UPDATE_PUBLIC_KEY:-WABHotBi7Ov3Wz2nZvNqFKw8zcaHngvg6/pkKStrpKQ=}"
UPDATE_FEED_BIN="${UPDATE_FEED_BIN:-update_feed}"
KEEP="${KEEP:-5}"

api="${GITEA_URL}/api/v1/repos/${REPO}"
auth="Authorization: token ${GITEA_TOKEN}"

mkdir -p "${DEST}/${channel}"
work="$(mktemp -d "${DEST}/.incoming.XXXXXX")"
trap 'rm -rf "${work}"' EXIT

# Find the release: the rolling `nightly` tag, or the newest non-prerelease.
if [ "${channel}" = "nightly" ]; then
  curl -sf -H "${auth}" "${api}/releases/tags/nightly" > "${work}/release.json"
else
  curl -sf -H "${auth}" "${api}/releases?limit=50" | python3 -c '
import json, sys
for release in json.load(sys.stdin):
    if not release.get("prerelease") and not release.get("draft"):
        print(json.dumps(release)); break
else:
    sys.exit("no stable release found")
' > "${work}/release.json"
fi

asset_url() {
  python3 -c '
import json, sys
name = sys.argv[1]
for asset in json.load(open(sys.argv[2]))["assets"]:
    if asset["name"] == name:
        print(asset["browser_download_url"]); break
else:
    sys.exit("release has no asset named " + name)
' "$1" "${work}/release.json"
}

curl -sfL -H "${auth}" -o "${work}/latest.json" "$(asset_url latest.json)"

read -r os arch dmg_name < <(python3 -c '
import json, os, sys
feed = json.load(open(sys.argv[1]))
print(feed["os"], feed["arch"], os.path.basename(feed["url"]))
' "${work}/latest.json")

if [ -f "${DEST}/${channel}/latest.json" ] && cmp -s "${work}/latest.json" "${DEST}/${channel}/latest.json"; then
  echo "${channel}: already up to date"
  exit 0
fi

curl -sfL -H "${auth}" -o "${work}/${dmg_name}" "$(asset_url "${dmg_name}")"

# Never publish what a client would reject.
"${UPDATE_FEED_BIN}" verify \
  --public-key "${PUBLIC_KEY}" \
  --channel "${channel}" --os "${os}" --arch "${arch}" \
  --file "${work}/${dmg_name}" \
  "${work}/latest.json"

# The build first, the feed that points at it last.
chmod 644 "${work}/${dmg_name}" "${work}/latest.json"
mv "${work}/${dmg_name}" "${DEST}/${channel}/${dmg_name}"
ln -sfn "${dmg_name}" "${DEST}/${channel}/Zed-xetk-${channel}-${arch}.dmg"
mv "${work}/latest.json" "${DEST}/${channel}/latest.json"
install -m 644 "$(dirname "$0")/index.html" "${DEST}/index.html"

# Keep the newest few builds so a client mid-update still finds its file.
prefix="${DEST}/${channel}/Zed-xetk-${channel}-"
# shellcheck disable=SC2012
ls -1t "${prefix}"[0-9]*"-${arch}.dmg" 2>/dev/null \
  | tail -n +"$((KEEP + 1))" | xargs -r rm -f --

echo "${channel}: published ${dmg_name}"
