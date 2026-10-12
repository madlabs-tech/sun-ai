#!/bin/sh
# Sun installer:
#
#   curl -fsSL https://get-sun.madlabs.tech | sudo sh        # Linux x86_64/arm64: systemd
#   curl -fsSL https://get-sun.madlabs.tech | sh             # macOS arm64: launchd agent
#
# Detects OS/arch, downloads the release, verifies its signature, installs
# `sun` and `sun-mod-sandbox` side by side, writes the service, starts
# `sun serve --with-host` and prints the UI URL and a sign-in link.
# A failed install says why and how to fix it, exits non-zero, and undoes
# everything it did.
#
# Options (after `sh -s --`):
#   --version V          release to install (default: latest)
#   --prefix DIR         binary dir (default /usr/local/bin; macOS ~/.local/bin)
#   --data-dir DIR       data dir (default /var/lib/sun; macOS
#                        ~/Library/Application Support/sun)
#   --no-service         install the binaries only; don't write or start a service
#   --download-only DIR  download + verify into DIR, nothing else (`sun upgrade`)
#   --from DIR           take the release files from DIR instead of downloading
# Env: SUN_INSTALL_URL (download base), SUN_RELEASE_KEY (trusted signer).
set -eu

# Download contract (get-sun.madlabs.tech redirects to the GitHub Release):
#   /                          this script
#   /releases/<version>/<file> <version> = latest or a bare semver (tag v<semver>)
SUN_INSTALL_URL=${SUN_INSTALL_URL:-https://get-sun.madlabs.tech}
SUN_RELEASE_KEY=${SUN_RELEASE_KEY:-"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEdlEdtxTJKj2DtLkku1zHt97rNmTD8gT9YNS83uw2Mb"}
SIGNER=release@madlabs.tech
NAMESPACE=sun-release

version=latest
prefix=""
data_dir=""
service=1
download_only=""
from=""

say() { printf 'sun-install: %s\n' "$*"; }
# fail <cause> <fix>
fail() {
  printf 'sun-install: error: %s\n' "$1" >&2
  printf 'sun-install: fix: %s\n' "$2" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=${2:?--version needs a value}; shift 2 ;;
    --prefix) prefix=${2:?--prefix needs a dir}; shift 2 ;;
    --data-dir) data_dir=${2:?--data-dir needs a dir}; shift 2 ;;
    --no-service) service=0; shift ;;
    --download-only) download_only=${2:?--download-only needs a dir}; shift 2 ;;
    --from) from=${2:?--from needs a dir}; shift 2 ;;
    *) fail "unknown option $1" "see the options at the top of this script" ;;
  esac
done
[ -n "$version" ] || version=latest
version=${version#v} # tags are v1.2.3; the URL takes 1.2.3

# ------------------------------------------------------------ undo on failure
tmp=$(mktemp -d "${TMPDIR:-/tmp}/sun-install.XXXXXX")
undo=""        # shell commands, newest first, run if we fail
done_ok=0
on_exit() {
  status=$?
  if [ "$done_ok" -ne 1 ] && [ -n "$undo" ]; then
    say "install failed; undoing changes"
    printf '%s\n' "$undo" | while IFS= read -r step; do
      [ -z "$step" ] || eval "$step" >/dev/null 2>&1 || true
    done
  fi
  rm -rf "$tmp"
  exit "$status"
}
trap on_exit EXIT
trap 'exit 1' INT TERM HUP
later() { undo="$1
$undo"; }
quote() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is not installed" "$2"; }

# ------------------------------------------------------------ platform
os=$(uname -s)
arch=$(uname -m)
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "unsupported CPU $arch" "Sun ships for x86_64 and arm64" ;;
esac
case "$os" in
  Linux) target="$arch-unknown-linux-musl" ;;
  Darwin)
    [ "$arch" = aarch64 ] || fail "Intel Macs are not supported" "use an Apple silicon Mac or a Linux box"
    target=aarch64-apple-darwin ;;
  *) fail "unsupported OS $os" "Sun runs on Linux and macOS" ;;
esac

# ------------------------------------------------------------ download
archives="sun-cli-$target.tar.xz sun-mod-$target.tar.xz"
if [ -n "$from" ]; then
  for f in $archives sha256.sum sha256.sum.sig; do
    cp "$from/$f" "$tmp/$f" 2>/dev/null || fail "$from/$f is missing" "point --from at a folder holding a whole release"
  done
else
  need curl "install curl (apt install curl)"
  base="$SUN_INSTALL_URL/releases/$version"
  say "downloading Sun $version for $target"
  for f in $archives sha256.sum sha256.sum.sig; do
    curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/$f" "$base/$f" ||
      fail "could not download $base/$f" "check the network and that release $version exists"
  done
fi

# ------------------------------------------------------------ verify
need ssh-keygen "install OpenSSH (apt install openssh-client); it checks the release signature"
printf '%s %s\n' "$SIGNER" "$SUN_RELEASE_KEY" >"$tmp/allowed_signers"
ssh-keygen -Y verify -f "$tmp/allowed_signers" -I "$SIGNER" -n "$NAMESPACE" \
  -s "$tmp/sha256.sum.sig" <"$tmp/sha256.sum" >/dev/null 2>&1 ||
  fail "the release signature does not verify" "do not install it; re-download, and report it if it persists"
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}
for f in $archives; do
  want=$(awk -v f="$f" '{ n = $2; sub(/^\*/, "", n) } n == f { print $1 }' "$tmp/sha256.sum")
  [ -n "$want" ] && [ "$want" = "$(sha256 "$tmp/$f")" ] ||
    fail "$f does not match the signed checksums" "do not install it; re-download"
done
say "signature and checksums ok"

need tar "install tar"
[ "$os" != Linux ] || need xz "install xz (apt install xz-utils)"
mkdir "$tmp/x"
for f in $archives; do
  tar -xJf "$tmp/$f" -C "$tmp/x" || fail "could not unpack $f" "install xz (apt install xz-utils)"
done
find_one() { find "$tmp/x" -type f -name "$1" | head -n 1; }
sun_bin=$(find_one sun)
sandbox_bin=$(find_one sun-mod-sandbox)
[ -n "$sun_bin" ] && [ -n "$sandbox_bin" ] || fail "the release is missing sun or sun-mod-sandbox" "report it"

if [ -n "$download_only" ]; then
  mkdir -p "$download_only"
  cp "$sun_bin" "$sandbox_bin" "$download_only/"
  done_ok=1
  say "downloaded to $download_only"
  exit 0
fi

# Local-only features: simulators and VMs, when this machine has
# Xcode, the Android SDK or Lima. `sun offers` prints nothing on a VPS
# (systemd) install.
offers() {
  extras=$(SUN_DATA_DIR="$data_dir" "$prefix/sun" offers 2>/dev/null) || return 0
  [ -n "$extras" ] || return 0
  say "On this machine:"
  printf '%s\n' "$extras" | while IFS= read -r line; do say "  $line"; done
}

# ------------------------------------------------------------ install
if [ "$os" = Linux ]; then
  [ -n "$prefix" ] || prefix=/usr/local/bin
  [ -n "$data_dir" ] || data_dir=/var/lib/sun
  if [ "$service" = 1 ] && [ "$(id -u)" != 0 ]; then
    fail "installing the systemd service needs root" "run: curl -fsSL $SUN_INSTALL_URL | sudo sh"
  fi
else
  [ -n "$prefix" ] || prefix="$HOME/.local/bin"
  [ -n "$data_dir" ] || data_dir="$HOME/Library/Application Support/sun"
fi

if [ ! -d "$prefix" ]; then
  mkdir -p "$prefix" || fail "cannot create $prefix" "pick another dir with --prefix"
  later "rm -rf $(quote "$prefix")"
fi
for b in sun sun-mod-sandbox; do
  src=$sun_bin
  [ "$b" = sun ] || src=$sandbox_bin
  dst="$prefix/$b"
  if [ -e "$dst" ]; then
    cp -p "$dst" "$tmp/old-$b"
    later "cp -p $(quote "$tmp/old-$b") $(quote "$dst")"
  else
    later "rm -f $(quote "$dst")"
  fi
  # Write beside, then rename: a running sun keeps its old file.
  cp "$src" "$dst.new" && chmod 0755 "$dst.new" && mv -f "$dst.new" "$dst" ||
    fail "cannot write $dst" "run with sudo, or pick a writable dir with --prefix"
done
say "installed $prefix/sun and $prefix/sun-mod-sandbox"

if [ ! -d "$data_dir" ]; then
  mkdir -p "$data_dir" || fail "cannot create $data_dir" "pick another dir with --data-dir"
  later "rm -rf $(quote "$data_dir")"
fi

if [ "$service" = 0 ]; then
  done_ok=1
  say "done. Start it with: SUN_DATA_DIR=\"$data_dir\" $prefix/sun serve --with-host"
  offers
  exit 0
fi

listen=127.0.0.1:7700
if [ "$os" = Linux ]; then
  need systemctl "use --no-service and run sun under your own supervisor"
  if ! id sun >/dev/null 2>&1; then
    useradd --system --home-dir "$data_dir" --shell /usr/sbin/nologin sun ||
      fail "cannot create the sun user" "check useradd, or use --no-service"
    later "userdel sun"
  fi
  chown sun:sun "$data_dir"
  chmod 0750 "$data_dir"
  unit=/etc/systemd/system/sun.service
  [ ! -e "$unit" ] || fail "$unit already exists" "upgrade with \`sun upgrade\`, or remove the old unit first"
  sed -e "s|/usr/local/bin/sun|$prefix/sun|g" -e "s|/var/lib/sun|$data_dir|g" \
    "$(find_one sun.service)" >"$unit"
  later "rm -f $unit; systemctl daemon-reload"
  systemctl daemon-reload
  systemctl enable --now sun >/dev/null 2>&1 || fail "systemd could not start sun" "see: journalctl -u sun -n 50"
  later "systemctl disable --now sun"
  logs="journalctl -u sun -n 50"
else
  plist="$HOME/Library/LaunchAgents/dev.sun.kernel.plist"
  [ ! -e "$plist" ] || fail "$plist already exists" "upgrade with \`sun upgrade\`, or remove the old agent first"
  mkdir -p "$HOME/Library/LaunchAgents"
  # Agents need the CLIs the user has (claude, codex, git) on PATH.
  path="$prefix:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  sed -e "s|@SUN_BIN@|$prefix/sun|g" -e "s|@SUN_DATA_DIR@|$data_dir|g" -e "s|@PATH@|$path|g" \
    "$(find_one dev.sun.kernel.plist)" >"$plist"
  later "rm -f $(quote "$plist")"
  launchctl bootstrap "gui/$(id -u)" "$plist" || fail "launchd could not start sun" "see: $data_dir/sun.log"
  later "launchctl bootout gui/$(id -u)/dev.sun.kernel"
  logs="tail -n 50 \"$data_dir/sun.log\""
fi

# ------------------------------------------------------------ first sign-in
say "waiting for the kernel"
n=0
until [ -s "$data_dir/kernel.json" ] && curl -s -o /dev/null "http://$listen/api/v1/tasks"; do
  n=$((n + 1))
  [ "$n" -le 60 ] || fail "the kernel did not come up within 30 s" "see: $logs"
  sleep 0.5
done
done_ok=1
token=$(sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$data_dir/kernel.json" | head -n 1)
say "Sun is running."
say "  UI:      http://$listen"
say "  Sign in: admin@sun.local, password $token (first run: you then choose a password, which retires this token, and set your email)"
say "  Locked out? \`sun user set-password <email>\`."
say "  Check the box with \`sun doctor\`."
offers
[ "$os" != Linux ] || say "  On a remote box: ssh -L 7700:$listen <host>, then open the link locally."
