#!/bin/bash
set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
"$SCRIPT_DIR/build.sh"

if [[ "$(uname)" == "Darwin" ]]; then
    cp "$(dirname "$SCRIPT_DIR")/target/release/nerve-tui" ~/.cargo/bin/
else
    cp "$(dirname "$SCRIPT_DIR")/target/release/nerve-tui" ~/.cargo/bin/ 2>/dev/null || \
    sudo cp "$(dirname "$SCRIPT_DIR")/target/release/nerve-tui" /usr/local/bin/
fi
echo "nerve-tui installed"
