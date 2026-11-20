#!/bin/sh
# Installs a ulei release for the current account, without root (ADR-0042 section 10).
#   deploy/install.sh RELEASE_DIR
# Checks every binary of RELEASE_DIR against its SHA256SUMS before copying anything, then puts the
# binaries into $HOME/.local/bin (mode 0755) and the user units beside this script into
# ${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user (mode 0644). A file already in place with the same
# bytes is left as it is. It never writes ulei.toml or a secret and never calls systemctl: it
# prints the commands for the operator. A failure exits 1 (2 for usage) with one line naming the
# step.
set -eu

fail() {
  echo "install.sh: $1: $(printf '%s' "$2" | tr '\n' ' ')" >&2
  exit 1
}

# Puts $1 at $2 with mode $3 through a rename in the target directory, so a running binary is
# replaced rather than rewritten; a regular file with the same bytes is left in place. On a failure
# it returns 1 with the tool's message in $reason.
place() {
  if [ -d "$2" ]; then
    reason="$2 is a directory"
    return 1
  fi
  if [ -f "$2" ] && [ ! -L "$2" ] && cmp -s "$1" "$2"; then
    reason=$(chmod "$3" "$2" 2>&1) || return 1
    echo "unchanged $2"
    return 0
  fi
  tmp="$2.install.$$"
  if ! reason=$( { cp "$1" "$tmp" && chmod "$3" "$tmp" && mv -f "$tmp" "$2"; } 2>&1 ); then
    rm -f "$tmp"
    return 1
  fi
  echo "installed $2"
}

if [ "$#" -ne 1 ]; then
  echo "usage: deploy/install.sh RELEASE_DIR" >&2
  exit 2
fi
release=$1
here=$(dirname "$0")

[ "$(id -u)" -ne 0 ] || fail account "run as the account that owns the deployment, not root"
case "${HOME:-}" in
  /*) ;;
  *) fail account "HOME is not an absolute path" ;;
esac
case "${XDG_CONFIG_HOME:-}" in
  /*) config=$XDG_CONFIG_HOME ;;
  *) config=$HOME/.config ;;
esac
bindir=$HOME/.local/bin
unitdir=$config/systemd/user
set -- ulei ulei-inbox ulei-engine ulei-web ulei-tg

sums=$release/SHA256SUMS
[ -f "$sums" ] || fail verify "no SHA256SUMS in $release"
[ "$(wc -l < "$sums")" -eq "$#" ] || fail verify "SHA256SUMS does not list exactly: $*"
for bin in "$@"; do
  [ -f "$release/$bin" ] || fail verify "$bin is missing in $release"
  want=$(awk -v name="$bin" '$2 == name || $2 == "*" name { print $1 }' "$sums")
  [ -n "$want" ] || fail verify "SHA256SUMS does not list $bin"
  [ -r "$release/$bin" ] || fail verify "cannot read $release/$bin"
  got=$(sha256sum < "$release/$bin" 2>&1) || fail verify "$got"
  [ "$want" = "${got%% *}" ] || fail verify "$bin does not match SHA256SUMS"
done
for unit in ulei-engine.service ulei-inbox.service ulei-tg.service; do
  [ -f "$here/systemd/$unit" ] || fail verify "$unit is missing in $here/systemd"
done

reason=$(mkdir -p "$bindir" 2>&1) || fail binaries "$reason"
for bin in "$@"; do
  place "$release/$bin" "$bindir/$bin" 0755 || fail binaries "$reason"
done

reason=$(mkdir -p "$unitdir" 2>&1) || fail units "$reason"
for unit in ulei-engine.service ulei-inbox.service ulei-tg.service; do
  place "$here/systemd/$unit" "$unitdir/$unit" 0644 || fail units "$reason"
done

if [ ! -f "$config/ulei/ulei.toml" ]; then
  echo "write $config/ulei/ulei.toml before the first start (see docs/ulei.example.toml)"
fi
cat <<EOF
next, as this account:
  systemctl --user daemon-reload
  systemctl --user enable --now ulei-engine.service ulei-inbox.service ulei-tg.service
  systemctl --user restart ulei-engine.service ulei-inbox.service ulei-tg.service  # on an upgrade
  $bindir/ulei status
EOF
