#!/bin/sh
# Installs a ulei release for the current account, without root (ADR-0042 section 10, ADR-0056
# section 6, ADR-0070, ADR-0071 section 8, ADR-0075, ADR-0078,
# ADR-0097, ADR-0101, ADR-0103).
#   deploy/install.sh RELEASE_DIR
#   deploy/install.sh --connector NAME --dir DIR RELEASE_DIR
#   deploy/install.sh --verify RELEASE_DIR
# Before copying anything, all three check every file RELEASE_DIR/SHA256SUMS lists against it, and
# that it lists every ulei binary and otherwise only the root helper ulei-priv and connector files
# connectors/<name>/<file>; the third stops there and writes nothing. The first puts the binaries into $HOME/.local/bin (mode 0755) and the
# user units beside this script into ${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user (mode 0644),
# no connector and not ulei-priv, which the owner installs as root (ADR-0073). The class slices
# hive-heads, hive-urgent and background get their caps appended (ADR-0103): CPUQuota a share of
# the CPUs (nproc), and the IOPS caps shares of the [host.io] table of ulei.toml, without which
# they get none; it says so, and whether the user manager has the io controller. The second
# puts the files of connectors/NAME into DIR/NAME: connector.json with mode 0644, the others 0755.
# A file already in place with the same bytes is left as it is. It never writes ulei.toml or a
# secret and never calls systemctl: it prints the next steps for the operator. A failure exits 1
# (2 for usage) with one line naming the step.
set -eu

fail() {
  echo "install.sh: $1: $(printf '%s' "$2" | tr '\n' ' ')" >&2
  exit 1
}

usage() {
  echo "usage: deploy/install.sh [--connector NAME --dir DIR | --verify] RELEASE_DIR" >&2
  exit 2
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

# Splits the SHA256SUMS line $1 into $want and $file.
entry() {
  want=${1%% *}
  file=${1#"$want" }
  file=${file#[ *]}
}

# Reads the table [host.io] of the ulei.toml $1 into $device, $read_iops and $write_iops, all
# empty without the table; only that table form is read, so a host key in another form, a missing
# key or a malformed value fails the install.
host_io() {
  device="" read_iops="" write_iops="" table=""
  [ -f "$1" ] || return 0
  found=$(awk '
    function bad(why) { print "!" why; exit }
    /^[ \t]*(#|$)/ { next }
    /^[ \t]*\[/ {
      table = $0
      sub(/^[ \t]*\[[ \t]*/, "", table); sub(/[ \t]*\].*$/, "", table)
      if (table == "host" || (table ~ /^host\./ && table != "host.io")) bad("[" table "] is not read; use the table [host.io]")
      if (table == "host.io") print "table=host.io"
      next
    }
    table == "" && /^[ \t]*host[ \t]*[.=]/ { bad("host keys are read only from the table [host.io]") }
    table != "host.io" { next }
    {
      line = $0
      key = line; sub(/^[ \t]*/, "", key); sub(/[ \t]*=.*$/, "", key)
      value = line; sub(/^[^=]*=[ \t]*/, "", value); sub(/[ \t]*(#.*)?$/, "", value)
      if (key == "device") {
        if (value !~ /^"\/dev\/[A-Za-z0-9._:\/-]+"$/) bad("host.io.device must be a quoted path under /dev/, got " value)
        gsub(/"/, "", value)
      } else if (key == "read_iops" || key == "write_iops") {
        if (value !~ /^[1-9][0-9]*$/ || length(value) > 8 || value + 0 > 10000000) bad("host.io." key " must be an integer 1 to 10000000, got " value)
      } else bad("host.io has no key " key)
      print key "=" value
    }
  ' "$1") || fail host.io "cannot read $1"
  for item in $found; do
    case "$item" in
      !*) fail host.io "$1: $(printf '%s' "$found" | sed -n 's/^!//p')" ;;
      table=*) table=${item#table=} ;;
      device=*) device=${item#device=} ;;
      read_iops=*) read_iops=${item#read_iops=} ;;
      write_iops=*) write_iops=${item#write_iops=} ;;
    esac
  done
  if [ -n "$table" ] && { [ -z "$device" ] || [ -z "$read_iops" ] || [ -z "$write_iops" ]; }; then
    fail host.io "$1: [host.io] needs device, read_iops and write_iops"
  fi
}

# Writes to $2 the class slice $1 of deploy/systemd with its caps appended: CPUQuota of $cpus
# CPUs and, with $device set, the IOPS shares of $read_iops and $write_iops; the percentages are
# ADR-0103's.
class_slice() {
  case "$1" in
    hive-heads.slice) cpu=50 read=33 write=20 ;;
    hive-urgent.slice) cpu=0 read=15 write=10 ;;
    background.slice) cpu=25 read=25 write=20 ;;
  esac
  {
    cat "$here/systemd/$1"
    if [ "$cpu" -gt 0 ]; then echo "CPUQuota=$((cpus * cpu))%"; fi
    if [ -n "$device" ]; then
      echo "IOReadIOPSMax=$device $(share "$read_iops" "$read")"
      echo "IOWriteIOPSMax=$device $(share "$write_iops" "$write")"
    fi
  } > "$2"
}

# $2 percent of $1, at least 1.
share() {
  n=$(($1 * $2 / 100))
  if [ "$n" -lt 1 ]; then n=1; fi
  echo "$n"
}

# Whether $1 is a C11 connector name.
is_name() {
  case "$1" in
    dialogue | [!a-z]* | *[!a-z0-9_-]*) return 1 ;;
  esac
  [ "${#1}" -ge 1 ] && [ "${#1}" -le 64 ]
}

connector="" dir="" connector_mode=0 dir_given=0 verify_mode=0
while [ "$#" -gt 1 ]; do
  case "$1" in
    --connector) [ "$#" -gt 2 ] || usage; connector=$2; connector_mode=1; shift 2 ;;
    --dir) [ "$#" -gt 2 ] || usage; dir=$2; dir_given=1; shift 2 ;;
    --verify) verify_mode=1; shift ;;
    *) usage ;;
  esac
done
if [ "$#" -ne 1 ] || [ "$connector_mode" -ne "$dir_given" ]; then usage; fi
if [ "$verify_mode" -eq 1 ] && [ "$connector_mode" -eq 1 ]; then usage; fi
case "$1" in
  -*) usage ;;
esac
release=$1
here=$(dirname "$0")
set -- ulei ulei-inbox ulei-engine ulei-web ulei-tg ulei-connectors ulei-memory ulei-hive-mcp \
  ulei-garden ulei-garden-embed
units="ulei-engine.service ulei-inbox.service ulei-tg.service ulei-connectors.service ulei-memory.service
  ulei-hive-mcp.service ulei-garden.service ulei-night.service ulei-night.timer ulei-uncertain.service
  ulei-uncertain.timer ulei-hive-watchdog.service ulei-hive-watchdog.timer ulei.slice"
slices="hive-heads.slice hive-urgent.slice background.slice"

[ "$(id -u)" -ne 0 ] || fail account "run as the account that owns the deployment, not root"
case "${HOME:-}" in
  /*) ;;
  *) fail account "HOME is not an absolute path" ;;
esac
if [ "$connector_mode" -eq 1 ]; then
  is_name "$connector" || fail connector "$connector is not a connector name"
  [ -n "$dir" ] || fail connector "the --dir value is empty"
fi

sums=$release/SHA256SUMS
[ -f "$sums" ] || fail verify "no SHA256SUMS in $release"
[ -r "$sums" ] || fail verify "cannot read $sums"
listed=" " released=" "
alien="which is neither a ulei binary, ulei-priv nor a file connectors/<name>/<file>"
while IFS= read -r line || [ -n "$line" ]; do
  entry "$line"
  case "$want" in
    *[!0-9a-f]*) fail verify "SHA256SUMS has a malformed line: $line" ;;
  esac
  [ "${#want}" -eq 64 ] || fail verify "SHA256SUMS has a malformed line: $line"
  case "$file" in
    connectors/*/*/* | connectors/*/.* | connectors/*/ | *[!A-Za-z0-9._/-]*)
      fail verify "SHA256SUMS lists $file, $alien" ;;
    connectors/*/*)
      name=${file#connectors/}
      name=${name%%/*}
      is_name "$name" || fail verify "SHA256SUMS lists $file, whose $name is not a connector name"
      base=${file##*/}
      if [ "$base" = connector.json ]; then released="$released$name "; fi ;;
    ulei-priv) ;;
    *)
      case " $* " in
        *" $file "*) listed="$listed$file " ;;
        *) fail verify "SHA256SUMS lists $file, $alien" ;;
      esac ;;
  esac
  [ -f "$release/$file" ] || fail verify "$file is missing in $release"
  [ -r "$release/$file" ] || fail verify "cannot read $release/$file"
  got=$(sha256sum < "$release/$file" 2>&1) || fail verify "$got"
  [ "$want" = "${got%% *}" ] || fail verify "$file does not match SHA256SUMS"
done < "$sums"
for bin in "$@"; do
  case "$listed" in
    *" $bin "*) ;;
    *) fail verify "SHA256SUMS does not list $bin" ;;
  esac
done
if [ "$verify_mode" -eq 1 ]; then
  echo "verified $release: SHA256SUMS matches every file it lists, among them the $# binaries this script installs: $*"
  exit 0
fi

if [ "$connector_mode" -eq 1 ]; then
  case "$released" in
    *" $connector "*) ;;
    *) fail connector "no connector $connector in $release (released:${released% })" ;;
  esac
  target=$dir/$connector
  reason=$(mkdir -p "$target" 2>&1) || fail connector "$reason"
  while IFS= read -r line || [ -n "$line" ]; do
    entry "$line"
    case "$file" in
      "connectors/$connector/"*) ;;
      *) continue ;;
    esac
    base=${file##*/}
    mode=0755
    if [ "$base" = connector.json ]; then mode=0644; fi
    place "$release/$file" "$target/$base" "$mode" || fail connector "$reason"
  done < "$sums"
  target=$(cd "$target" && pwd -P) || fail connector "cannot resolve $target"
  cat <<EOF
next, in ulei.toml (see docs/ulei.example.toml), to connect $connector:
  [connectors] dir = "${target%/*}", "$connector" in enabled, with state, log and run_dir
  [memory] socket, where ulei-memory serves C10
  [connectors.$connector] key_file, allow, read, write, network or landlock only to grant it more
then, as this account:
  $HOME/.local/bin/ulei-connectors list
  systemctl --user restart ulei-connectors.service
EOF
  exit 0
fi

case "${XDG_CONFIG_HOME:-}" in
  /*) config=$XDG_CONFIG_HOME ;;
  *) config=$HOME/.config ;;
esac
bindir=$HOME/.local/bin
unitdir=$config/systemd/user
for unit in $units $slices; do
  [ -f "$here/systemd/$unit" ] || fail verify "$unit is missing in $here/systemd"
done
host_io "$config/ulei/ulei.toml"
cpus=$(nproc 2>&1) || fail slices "nproc: $cpus"

reason=$(mkdir -p "$bindir" 2>&1) || fail binaries "$reason"
for bin in "$@"; do
  place "$release/$bin" "$bindir/$bin" 0755 || fail binaries "$reason"
done

reason=$(mkdir -p "$unitdir" 2>&1) || fail units "$reason"
for unit in $units; do
  place "$here/systemd/$unit" "$unitdir/$unit" 0644 || fail units "$reason"
done
for slice in $slices; do
  made="$unitdir/.$slice.$$"
  reason=$(class_slice "$slice" "$made" 2>&1) || { rm -f "$made"; fail slices "$reason"; }
  place "$made" "$unitdir/$slice" 0644 || { rm -f "$made"; fail slices "$reason"; }
  rm -f "$made"
done
if [ -z "$device" ]; then
  echo "no [host.io] in $config/ulei/ulei.toml: the class slices have no IOPS caps until it names the device and its read and write IOPS and this script runs again (docs/runbooks/io-classes.md)"
fi
uid=$(id -u)
controllers="/sys/fs/cgroup/user.slice/user-$uid.slice/user@$uid.service/cgroup.controllers"
if [ ! -r "$controllers" ]; then
  echo "cannot read $controllers: whether the user manager has the io controller is unknown, and without it the IOPS caps take no effect (docs/runbooks/io-classes.md)"
else
  case " $(cat "$controllers") " in
    *" io "*) ;;
    *) echo "the user manager has no io controller ($controllers): the IOPS caps take no effect until io is delegated to user@.service (docs/runbooks/io-classes.md)" ;;
  esac
fi

if [ ! -f "$config/ulei/ulei.toml" ]; then
  echo "write $config/ulei/ulei.toml before the first start (see docs/ulei.example.toml)"
fi
cat <<EOF
next, as this account:
  systemctl --user daemon-reload
  systemctl --user enable --now ulei-engine.service ulei-inbox.service ulei-tg.service ulei-uncertain.timer
  systemctl --user restart ulei-engine.service ulei-inbox.service ulei-tg.service  # on an upgrade
  $bindir/ulei status
the root helper ulei-priv is not installed here: the owner installs it as root to
/usr/local/lib/ulei/ (docs/runbooks/mvp-pilot-cutover.md part 5.2)
the archive, once ulei.toml has [memory] (docs/runbooks/memory-archive.md):
  systemctl --user enable --now ulei-memory.service
  systemctl --user try-restart ulei-memory.service  # on an upgrade
the connector runner, once ulei.toml has [connectors] and memory.socket serves C10:
  systemctl --user enable --now ulei-connectors.service
  systemctl --user try-restart ulei-connectors.service  # on an upgrade
the hive tools for the engine, once ulei.toml has [hive_mcp] and a drop-in names the hive's root
(docs/runbooks/mvp-pilot-cutover.md):
  systemctl --user enable --now ulei-hive-mcp.service
  systemctl --user try-restart ulei-hive-mcp.service  # on an upgrade
the hive's stall watchdog, once $config/ulei/hive-watchdog.env names the hive (HIVE_CLI,
HIVE_CONFIG), a drop-in its root and the hive's config has [watchdog]
(docs/runbooks/mvp-pilot-cutover.md part 7):
  systemctl --user enable --now ulei-hive-watchdog.timer
the garden, once ulei.toml has [garden] (docs/runbooks/garden-service.md); the embedder
ulei-garden-embed also needs the weight file its convert writes, which this script does not install:
  systemctl --user enable --now ulei-garden.service
  systemctl --user try-restart ulei-garden.service  # on an upgrade
the night gardener, once ulei.toml has [night] and the garden runs; a dry run first
(docs/runbooks/night-gardener.md):
  ULEI_NIGHT_STATE=\${XDG_STATE_HOME:-\$HOME/.local/state}/ulei/night $bindir/ulei-engine night --dry-run
  systemctl --user enable --now ulei-night.timer
EOF
