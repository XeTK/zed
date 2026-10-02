#!/usr/bin/env bash
# Pushes a signed build to the update host (zed.xetk.co.uk) over rsync, as the
# rrsync-restricted `zed-publish` account: that account can only write inside
# /mnt/shared/zed-updates on LXC 100, whatever it is asked to run.
#
#   publish.sh <nightly|stable> <arch> <dmg> <latest.json>
#
# Environment:
#   ZED_PUBLISH_SSH_KEY   private key for the account (required)
#   ZED_PUBLISH_HOST_KEY  the server's pinned host key, "ssh-ed25519 AAAA..."
#                         (required; no trust-on-first-use)
#   ZED_PUBLISH_HOST      default 10.0.5.2
#   ZED_PUBLISH_USER      default zed-publish
#
# The build goes up before the feed that points at it, so a client never sees
# a feed for a file that is not there yet. Old builds are pruned on the host by
# a systemd timer (xetk/infra), since this account cannot delete.
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <nightly|stable> <arch> <dmg> <latest.json>" >&2
  exit 2
fi
channel="$1"
arch="$2"
dmg="$3"
feed="$4"

case "${channel}" in
  nightly | stable) ;;
  *) echo "unknown channel: ${channel}" >&2; exit 2 ;;
esac

: "${ZED_PUBLISH_SSH_KEY:?set ZED_PUBLISH_SSH_KEY}"
: "${ZED_PUBLISH_HOST_KEY:?set ZED_PUBLISH_HOST_KEY}"
host="${ZED_PUBLISH_HOST:-10.0.5.2}"
user="${ZED_PUBLISH_USER:-zed-publish}"

# act_runner runs jobs with a restricted PATH, so rsync and ssh are invoked by
# absolute path (the same reason anti-vocale's F-Droid publish does).
rsync_bin="${RSYNC:-/usr/bin/rsync}"
ssh_bin="${SSH:-/usr/bin/ssh}"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

printf '%s\n' "${ZED_PUBLISH_SSH_KEY}" > "${work}/key"
chmod 600 "${work}/key"
printf '%s %s\n' "${host}" "${ZED_PUBLISH_HOST_KEY}" > "${work}/known_hosts"
ssh_command="${ssh_bin} -i ${work}/key -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=${work}/known_hosts"

name="$(basename "${dmg}")"
stage="${work}/stage"
mkdir -p "${stage}/${channel}"
cp "${dmg}" "${stage}/${channel}/${name}"
# The "newest build" alias brew and the landing page use.
ln -s "${name}" "${stage}/${channel}/Zed-xetk-${channel}-${arch}.dmg"
cp "${feed}" "${stage}/${channel}/latest.json"
cp "$(dirname "$0")/index.html" "${stage}/index.html"
# World-readable for nginx. Set here and sent with -p, because the openrsync
# macOS ships as /usr/bin/rsync does not understand --chmod.
chmod 755 "${stage}" "${stage}/${channel}"
chmod 644 "${stage}/${channel}/${name}" "${stage}/${channel}/latest.json" "${stage}/index.html"

destination="${user}@${host}"

# 1. The build and its alias, not yet the feed.
"${rsync_bin}" -rltp --exclude latest.json -e "${ssh_command}" \
  "${stage}/${channel}/" "${destination}:${channel}/"

# 2. The feed, which makes the new build visible to clients.
"${rsync_bin}" -tp -e "${ssh_command}" \
  "${stage}/${channel}/latest.json" "${destination}:${channel}/"

# 3. The landing page.
"${rsync_bin}" -tp -e "${ssh_command}" \
  "${stage}/index.html" "${destination}:"

echo "published ${channel}: ${name}"
