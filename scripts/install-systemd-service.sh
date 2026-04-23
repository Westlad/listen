#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
uid="$(id -u)"
user="${USER:?USER is not set}"

if [[ "$uid" == "0" ]]; then
  echo "Run this script as the target user, not with sudo. The script will use sudo only for installation steps." >&2
  exit 1
fi

cargo build --release --features audio-cpal --manifest-path "$repo_root/Cargo.toml"

render() {
  local source="$1"
  sed \
    -e "s|@REPO_ROOT@|$repo_root|g" \
    -e "s|@USER@|$user|g" \
    -e "s|@UID@|$uid|g" \
    "$source"
}

tmp_unit="$(mktemp)"
tmp_tmpfiles="$(mktemp)"
trap 'rm -f "$tmp_unit" "$tmp_tmpfiles"' EXIT

render "$repo_root/systemd/openclaw-listen.service.in" > "$tmp_unit"
render "$repo_root/systemd/openclaw-listen.tmpfiles.in" > "$tmp_tmpfiles"

sudo install -m 0644 "$tmp_unit" /etc/systemd/system/openclaw-listen.service
sudo install -m 0644 "$tmp_tmpfiles" /etc/tmpfiles.d/openclaw-listen.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/openclaw-listen.conf
sudo systemctl daemon-reload

cat <<EOF
Installed openclaw-listen.service for $user.

Next steps:
  sudo systemctl enable --now openclaw-listen.service
  sudo systemctl status openclaw-listen.service
EOF
