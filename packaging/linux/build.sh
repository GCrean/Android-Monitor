#!/usr/bin/env bash
# Build the Linux release artifacts: .deb and .tar.gz  ->  dist/
# Usage: packaging/linux/build.sh [version]     (needs: cargo, cargo-deb, dev libs from README)
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
mkdir -p dist

cargo build --release
BIN="${CARGO_TARGET_DIR:-target}/release/android-monitor"

# .deb
cargo deb --no-build --deb-version "$VERSION" --output "dist/android-monitor_${VERSION}_amd64.deb"

# .tar.gz (binary + desktop entry + icons)
STAGE="$(mktemp -d)/android-monitor-${VERSION}-linux-x64"
mkdir -p "$STAGE/icons"
cp "$BIN" "$STAGE/"
cp packaging/linux/android-monitor.desktop assets/icon.png README.md LICENSE "$STAGE/"
cp assets/icons/*.png "$STAGE/icons/"
cat > "$STAGE/install.sh" <<'EOF'
#!/usr/bin/env bash
# Installs for the current user into ~/.local
set -e
cd "$(dirname "$0")"
install -Dm755 android-monitor "$HOME/.local/bin/android-monitor"
install -Dm644 android-monitor.desktop "$HOME/.local/share/applications/android-monitor.desktop"
for f in icons/*x*.png; do s=$(basename "$f" .png); install -Dm644 "$f" "$HOME/.local/share/icons/hicolor/$s/apps/android-monitor.png"; done
echo "Installed. Make sure adb is on your PATH."
EOF
chmod +x "$STAGE/install.sh"
tar -C "$(dirname "$STAGE")" -czf "dist/android-monitor-${VERSION}-linux-x64.tar.gz" "$(basename "$STAGE")"

ls -lh dist/*linux* dist/*.deb
