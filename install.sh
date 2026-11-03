#!/bin/sh
# Install VMDesk as a desktop app for the current user:
#   ./install.sh            build the release binaries and install them
#   ./install.sh uninstall  remove what install put in place
#
# The binaries (vmdesk-app, vmdesk and, when the musl target is installed, the static agent
# vmdesk-agent-x86_64) go to ~/.local/bin side by side, since the app runs the CLI and
# uploads the agent from its own directory. The launcher goes to
# ~/.local/share/applications and the icons to ~/.local/share/icons/hicolor. Your data in
# ~/.local/share/vmdesk stays either way.
set -eu

cd "$(dirname "$0")"
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
apps_dir="$data_dir/applications"
icons_dir="$data_dir/icons/hicolor"
sizes="16 24 32 48 64 128 256 512"

refresh() {
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database -q "$apps_dir" || true
    fi
    if command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -q -t "$icons_dir" || true
    fi
}

if [ "${1:-}" = uninstall ]; then
    rm -f "$bin_dir/vmdesk-app" "$bin_dir/vmdesk" "$bin_dir/vmdesk-agent-x86_64" \
        "$apps_dir/vmdesk.desktop" "$icons_dir/scalable/apps/vmdesk.svg"
    for size in $sizes; do
        rm -f "$icons_dir/${size}x${size}/apps/vmdesk.png"
    done
    refresh
    echo "Removed VMDesk. Your VMs and history in $data_dir/vmdesk are kept."
    exit 0
fi

if [ -n "${1:-}" ]; then
    echo "usage: $0 [uninstall]" >&2
    exit 2
fi

# The embedded terminal links libghostty-vt; say how to get it rather than fail deep in
# the build.
if ! PKG_CONFIG_PATH="${PKG_CONFIG_PATH:+$PKG_CONFIG_PATH:}$data_dir/pkgconfig" \
    pkg-config --exists libghostty-vt 2>/dev/null; then
    cat >&2 <<'MSG'
libghostty-vt is not installed. Build it once from Ghostty's source (needs Zig):
  git clone https://github.com/ghostty-org/ghostty && cd ghostty
  zig build -Demit-lib-vt -Doptimize=ReleaseFast --prefix ~/.local
Then run ./install.sh again.
MSG
    exit 1
fi

cargo build --release -p vmdesk-app -p vmdesk-daemon
install -Dm755 target/release/vmdesk-app "$bin_dir/vmdesk-app"
install -Dm755 target/release/vmdesk "$bin_dir/vmdesk"

if rustup target list --installed 2>/dev/null | grep -qx x86_64-unknown-linux-musl; then
    cargo build -p vmdesk-agent --profile agent --target x86_64-unknown-linux-musl
    install -Dm755 target/x86_64-unknown-linux-musl/agent/vmdesk-agent \
        "$bin_dir/vmdesk-agent-x86_64"
else
    echo "Skipped the on-VM agent: run 'rustup target add x86_64-unknown-linux-musl' and" \
        "install again to be able to install agents from the app." >&2
fi

install -Dm644 packaging/vmdesk.svg "$icons_dir/scalable/apps/vmdesk.svg"
for size in $sizes; do
    install -Dm644 "packaging/icons/vmdesk-$size.png" "$icons_dir/${size}x${size}/apps/vmdesk.png"
done
# Point the launcher at the installed binary: ~/.local/bin is not always on the desktop
# session's PATH.
mkdir -p "$apps_dir"
sed "s|^Exec=vmdesk-app|Exec=$bin_dir/vmdesk-app|" packaging/vmdesk.desktop >"$apps_dir/vmdesk.desktop"
chmod 644 "$apps_dir/vmdesk.desktop"
refresh

echo "Installed VMDesk to $bin_dir. Find it as \"VMDesk\" in your apps."
case ":$PATH:" in
*":$bin_dir:"*) ;;
*) echo "Add $bin_dir to your PATH to use the vmdesk command." ;;
esac
