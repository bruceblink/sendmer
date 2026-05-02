#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$ROOT_DIR" ]; then
  echo "Error: not inside a git repository."
  exit 1
fi

HOOKS_DIR="$ROOT_DIR/.git/hooks"
HOOK_FILE="$HOOKS_DIR/pre-commit"

mkdir -p "$HOOKS_DIR"

cat > "$HOOK_FILE" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

echo "[pre-commit] running cargo fmt check..."
cargo fmt --all -- --check

echo "[pre-commit] running cargo clippy check..."
cargo clippy --locked --workspace --all-targets --all-features

echo "[pre-commit] checks passed."
EOF

chmod +x "$HOOK_FILE"

echo "Installed pre-commit hook at: $HOOK_FILE"
