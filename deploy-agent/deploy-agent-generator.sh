#!/bin/bash
# /etc/systemd/system-generators/deploy-agent-generator
set -euo pipefail

TARGET_DIR="$1"
UNITS_BASE="/etc/deploy-agent/units"

if [ -d "$UNITS_BASE" ]; then
    find "$UNITS_BASE" -mindepth 2 -maxdepth 2 -type f \( -name "*.service" -o -name "*.timer" -o -name "*.socket" \) | while read -r unit_path; do
        unit_name=$(basename "$unit_path")
        ln -sfn "$unit_path" "$TARGET_DIR/$unit_name"
    done
fi
