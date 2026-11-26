#!/bin/sh
# Installs a ulei release for the current account, without root (ADR-0042 section 10, ADR-0056
# section 6, ADR-0070, ADR-0071 section 8, ADR-0075, ADR-0078).
#   deploy/install.sh RELEASE_DIR
#   deploy/install.sh --connector NAME --dir DIR RELEASE_DIR
# Before copying anything, both check every file RELEASE_DIR/SHA256SUMS lists against it, and that
# it lists every ulei binary and otherwise only the root helper ulei-priv and connector files
# connectors/<name>/<file>. The first puts the binaries into $HOME/.local/bin (mode 0755) and the
# user units beside this script into ${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user (mode 0644),
# no connector and not ulei-priv, which the owner installs as root (ADR-0073). The second
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
  echo "usage: deploy/install.sh [--connector NAME --dir DIR] RELEASE_DIR" >&2
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

# Whether $1 is a C11 connector name.
is_name() {
  case "$1" in
    dialogue | [!a-z]* | *[!a-z0-9_-]*) return 1 ;;
  esac
  [ "${#1}" -ge 1 ] && [ "${#1}" -le 64 ]
}

connector="" dir="" connector_mode=0 dir_given=0
while [ "$#" -gt 1 ]; do
  case "$1" in
    --connector) [ "$#" -gt 2 ] || usage; connector=$2; connector_mode=1; shift 2 ;;
    --dir) [ "$#" -gt 2 ] || usage; dir=$2; dir_given=1; shift 2 ;;
    *) usage ;;
  esac
done
if [ "$#" -ne 1 ] || [ "$connector_mode" -ne "$dir_given" ]; then usage; fi
case "$1" in
  -*) usage ;;
esac
release=$1
here=$(dirname "$0")
set -- ulei ulei-inbox ulei-engine ulei-web ulei-tg ulei-connectors ulei-memory ulei-hive-mcp \
  ulei-garden ulei-garden-embed
units="ulei-engine.service ulei-inbox.service ulei-tg.service ulei-connectors.service ulei-memory.service
  ulei-hive-mcp.service ulei-garden.service ulei-night.service ulei-night.timer"

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
for unit in $units; do
  [ -f "$here/systemd/$unit" ] || fail verify "$unit is missing in $here/systemd"
done

reason=$(mkdir -p "$bindir" 2>&1) || fail binaries "$reason"
for bin in "$@"; do
  place "$release/$bin" "$bindir/$bin" 0755 || fail binaries "$reason"
done

reason=$(mkdir -p "$unitdir" 2>&1) || fail units "$reason"
for unit in $units; do
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
the garden, once ulei.toml has [garden] (docs/runbooks/garden-service.md); the embedder
ulei-garden-embed also needs ONNX Runtime and the model, which this script does not install:
  systemctl --user enable --now ulei-garden.service
  systemctl --user try-restart ulei-garden.service  # on an upgrade
the night gardener, once ulei.toml has [night] and the garden runs; a dry run first
(docs/runbooks/night-gardener.md):
  ULEI_NIGHT_STATE=\${XDG_STATE_HOME:-\$HOME/.local/state}/ulei/night $bindir/ulei-engine night --dry-run
  systemctl --user enable --now ulei-night.timer
EOF
