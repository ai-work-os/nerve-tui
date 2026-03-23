#!/bin/bash
set -e
cd "$(dirname "$0")/.."
cargo build --release
echo "nerve-tui built: target/release/nerve-tui"
