#!/bin/bash
# /etc/systemd/system-generators/deploy-agent-generator
set -euo pipefail

NORMAL_DIR="$1"
UNITS_DIR="/etc/deploy-agent/units"
MARKER_COMMENT="# deploy-agent-generator managed symlink"

mkdir -p "$NORMAL_DIR"

# Build the desired set: every *.service/*.timer file under UNITS_DIR.
declare -A desired
if [ -d "$UNITS_DIR" ]; then
    while IFS= read -r unit_file; do
        filename=$(basename "$unit_file")
        desired["$filename"]="$unit_file"
    done < <(find "$UNITS_DIR" -type f \( -name "*.service" -o -name "*.timer" \))
fi

# Add/update symlinks for everything desired.
for filename in "${!desired[@]}"; do
    ln -sfn "${desired[$filename]}" "$NORMAL_DIR/$filename"
done

# Remove symlinks we own that no longer have a source — only touch
# symlinks that actually point back into UNITS_DIR, never anything else
# that might legitimately live in $NORMAL_DIR from another generator.
for existing in "$NORMAL_DIR"/*.service "$NORMAL_DIR"/*.timer; do
    [ -e "$existing" ] || continue
    [ -L "$existing" ] || continue
    target=$(readlink -f "$existing" 2>/dev/null || true)
    filename=$(basename "$existing")
    case "$target" in
        "$UNITS_DIR"/*)
            if [ -z "${desired[$filename]:-}" ]; then
                rm -f "$existing"
            fi
            ;;
    esac
done

