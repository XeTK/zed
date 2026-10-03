#!/usr/bin/env bash
# Pushes a signed build to the update host (zed.xetk.co.uk) over rsync, as the
# rrsync-restricted `zed-publish` account: that account can only write inside
# /mnt/shared/zed-updates on LXC 100, whatever it is asked to run.
#
#   publish.sh <nightly|stable> <os>-<arch> <build file> <latest.json>
#
# <os>-<arch> is the platform in the same words the app and the feed use
# (std::env::consts::OS and ARCH), e.g. macos-aarch64 or linux-x86_64, and the
# build file is whatever that platform ships (.dmg, .tar.gz, .exe).
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
  echo "usage: $0 <nightly|stable> <os>-<arch> <build file> <latest.json>" >&2
  exit 2
fi
channel="$1"
platform="$2"
build="$3"
feed="$4"

case "${channel}" in
  nightly | stable) ;;
  *) echo "unknown channel: ${channel}" >&2; exit 2 ;;
esac

case "${platform}" in
  *[!a-z0-9_-]* | -* | *- | *-*-*) echo "platform must look like <os>-<arch>: ${platform}" >&2; exit 2 ;;
  *-*) ;;
  *) echo "platform must look like <os>-<arch>: ${platform}" >&2; exit 2 ;;
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

name="$(basename "${build}")"
case "${name}" in
  *.tar.gz) extension="tar.gz" ;;
  *.*) extension="${name##*.}" ;;
  *) echo "build file has no extension: ${name}" >&2; exit 2 ;;
esac

stage="${work}/stage"
platform_dir="${channel}/${platform}"
mkdir -p "${stage}/${platform_dir}"
cp "${build}" "${stage}/${platform_dir}/${name}"
# The "newest build" alias brew and the landing page use.
ln -s "${name}" "${stage}/${platform_dir}/Zed-xetk-${channel}-${platform}.${extension}"
cp "${feed}" "${stage}/${platform_dir}/latest.json"
# Builds made before feeds were per platform fetch /<channel>/latest.json.
# Only macOS on Apple silicon ever shipped that way, and its feed names the
# new download URL, so the same signed bytes serve those copies too.
legacy_feed=""
if [ "${platform}" = "macos-aarch64" ]; then
  legacy_feed="${channel}/latest.json"
  cp "${feed}" "${stage}/${legacy_feed}"
fi
cp "$(dirname "$0")/index.html" "${stage}/index.html"
cp "$(dirname "$0")/features.html" "${stage}/features.html"
cp "$(dirname "$0")/features.json" "${stage}/features.json"
# World-readable for nginx. Set here and sent with -p, because the openrsync
# macOS ships as /usr/bin/rsync does not understand --chmod.
chmod 755 "${stage}" "${stage}/${channel}" "${stage}/${platform_dir}"
chmod 644 "${stage}/${platform_dir}/${name}" "${stage}/${platform_dir}/latest.json" "${stage}/index.html" "${stage}/features.html" "${stage}/features.json"
if [ -n "${legacy_feed}" ]; then
  chmod 644 "${stage}/${legacy_feed}"
fi

destination="${user}@${host}"

# 1. The build and its alias, not yet any feed. Syncing from the root creates
# <channel>/<platform>/ on a host that has never seen this platform.
"${rsync_bin}" -rltp --exclude latest.json --exclude index.html --exclude features.html --exclude features.json -e "${ssh_command}" \
  "${stage}/" "${destination}:"

# 2. The feed, which makes the new build visible to clients.
"${rsync_bin}" -tp -e "${ssh_command}" \
  "${stage}/${platform_dir}/latest.json" "${destination}:${platform_dir}/"
if [ -n "${legacy_feed}" ]; then
  "${rsync_bin}" -tp -e "${ssh_command}" \
    "${stage}/${legacy_feed}" "${destination}:${channel}/"
fi

# 3. The landing page and the features page it links to.
"${rsync_bin}" -tp -e "${ssh_command}" \
  "${stage}/index.html" "${stage}/features.html" "${stage}/features.json" "${destination}:"

echo "published ${channel} ${platform}: ${name}"
