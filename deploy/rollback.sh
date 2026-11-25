#!/bin/sh
# Returns the owner inbox and the owner bot from ulei to the old consumer, without root
# (docs/runbooks/mvp-pilot-cutover.md, part 3).
#   deploy/rollback.sh OLD_UNIT
# OLD_UNIT is the user unit of the old runtime that the cutover stopped and disabled. Before it
# changes anything the script checks ${XDG_CONFIG_HOME:-$HOME/.config}/ulei/ulei.toml, its
# [inbox] root with events.sqlite3, that it can write both, that OLD_UNIT loads, and whether
# ulei-hive-mcp.service is installed, so a refused run leaves the host as it was. Then, in order:
# it stops and disables the three ulei units, which releases consumer.lock, and
# ulei-hive-mcp.service, so nothing of ulei serves the hive tools (a host without that unit, its
# LoadState not-found, gets a note and only the three); sets inbox.deliver = false; makes sure
# <root>/ENABLED exists; enables and starts OLD_UNIT, whose consumer takes the lock at its start.
# It deletes nothing, and a second run changes nothing more. A failure exits 1 (2 for usage)
# with one line naming the step.
set -eu

fail() {
  echo "rollback.sh: $1: $(printf '%s' "$2" | tr '\n' ' ')" >&2
  exit 1
}

# The awk rule that keeps the name of the config's current [section] in `section`.
# shellcheck disable=SC2016 # awk program text
sections='/^[ \t]*\[/ {
  section = $0; sub(/^[ \t]*\[[ \t]*/, "", section); sub(/[ \t]*\].*$/, "", section) }'

# Prints the string value of key $2 in section [$1] of the config $3; the config syntax has no
# escapes in strings.
value() {
  awk -v want="$1" -v key="$2" "$sections"'
    section == want && $0 ~ "^[ \t]*" key "[ \t]*=[ \t]*\"" {
      v = $0; sub(/^[^"]*"/, "", v); sub(/".*$/, "", v); print v; exit
    }' "$3"
}

if [ "$#" -ne 1 ]; then
  echo "usage: deploy/rollback.sh OLD_UNIT" >&2
  exit 2
fi
old=$1
case "$old" in
  ulei-*|-*|*/*|'')
    echo "usage: deploy/rollback.sh OLD_UNIT (the old runtime's unit, not a ulei unit)" >&2
    exit 2 ;;
esac
set -- ulei-tg.service ulei-inbox.service ulei-engine.service
hive=ulei-hive-mcp.service
umask 077

[ "$(id -u)" -ne 0 ] || fail account "run as the account that owns the deployment, not root"
case "${HOME:-}" in
  /*) ;;
  *) fail account "HOME is not an absolute path" ;;
esac
case "${XDG_CONFIG_HOME:-}" in
  /*) config=$XDG_CONFIG_HOME/ulei/ulei.toml ;;
  *) config=$HOME/.config/ulei/ulei.toml ;;
esac

[ -f "$config" ] || fail check "no config $config"
[ -r "$config" ] || fail check "cannot read $config"
[ -w "${config%/*}" ] || fail check "cannot write ${config%/*}"
root=$(value inbox root "$config") || fail check "cannot read $config"
case "$root" in
  /*) ;;
  *) fail check "$config has no absolute [inbox] root" ;;
esac
[ -f "$root/events.sqlite3" ] || fail check "no events.sqlite3 in the inbox root $root"
if [ -e "$root/ENABLED" ]; then
  [ -f "$root/ENABLED" ] || fail check "$root/ENABLED is not a file"
else
  [ -w "$root" ] || fail check "cannot write $root"
fi
state=$(systemctl --user show -p LoadState --value -- "$old" 2>&1) || fail check "$state"
[ "$state" = loaded ] || fail check "$old is $state, not a loaded unit"
state=$(systemctl --user show -p LoadState --value -- "$hive" 2>&1) || fail check "$state"
case "$state" in
  not-found) skipped="skipped $hive: not installed" ;;
  '') fail check "$hive has no LoadState" ;;
  *) skipped=; set -- "$@" "$hive" ;;
esac

[ -z "$skipped" ] || echo "$skipped"

reason=$(systemctl --user disable --now "$@" 2>&1) || fail stop "$reason"
echo "stopped and disabled $*"

tmp=$config.rollback.$$
if ! reason=$(awk "$sections"'
    section == "inbox" && /^[ \t]*deliver[ \t]*=/ { print "deliver = false"; next }
    { print }' "$config" 2>&1 > "$tmp"); then
  rm -f "$tmp"
  fail config "$reason"
fi
if cmp -s "$tmp" "$config"; then
  rm -f "$tmp"
  echo "unchanged $config: inbox.deliver is not true"
else
  reason=$(mv -f "$tmp" "$config" 2>&1) || { rm -f "$tmp"; fail config "$reason"; }
  echo "set inbox.deliver = false in $config"
fi

if [ -f "$root/ENABLED" ]; then
  echo "unchanged $root/ENABLED"
else
  reason=$( { : > "$root/ENABLED"; } 2>&1) || fail gate "$reason"
  echo "created $root/ENABLED"
fi

reason=$(systemctl --user enable --now -- "$old" 2>&1) || fail start "$reason"
echo "enabled and started $old"
