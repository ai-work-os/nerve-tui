#!/bin/bash
set -e
cd "$(dirname "$0")/.."
git pull
"$(dirname "$0")/install.sh"
echo "nerve-tui updated and installed"
