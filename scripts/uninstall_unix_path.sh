#!/bin/sh
set -eu

dry_run=0
profile_path="${LULLABY_PROFILE:-${HOME:-}/.profile}"
config_home="${XDG_CONFIG_HOME:-${HOME:-}/.config}"
env_dir="$config_home/lullaby"
env_file="$env_dir/env"

usage() {
    echo "usage: ./uninstall.sh [--dry-run] [--profile path]" >&2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run)
            dry_run=1
            shift
            ;;
        --profile)
            if [ "$#" -lt 2 ]; then
                usage
                exit 2
            fi
            profile_path=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

if [ -z "${HOME:-}" ]; then
    echo "HOME is not set; cannot remove a user PATH helper." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
package_root=$script_dir
bin_path="$package_root/bin"
source_line=". \"$env_file\""

if [ "$dry_run" -eq 1 ]; then
    echo "dry-run: would remove source line from profile if present: $profile_path"
    echo "dry-run: would remove PATH environment file if it points at: $bin_path"
    exit 0
fi

if [ -f "$profile_path" ]; then
    temp_profile="${profile_path}.lullaby.tmp"
    grep -Fxv "$source_line" "$profile_path" > "$temp_profile" || true
    mv "$temp_profile" "$profile_path"
    echo "Updated shell profile: $profile_path"
else
    echo "Shell profile was not present: $profile_path"
fi

if [ -f "$env_file" ] && grep -F "$bin_path" "$env_file" >/dev/null 2>&1; then
    rm -f "$env_file"
    rmdir "$env_dir" 2>/dev/null || true
    echo "Removed Lullaby PATH environment file: $env_file"
elif [ -f "$env_file" ]; then
    echo "Left PATH environment file in place because it does not point at this package: $env_file"
else
    echo "Lullaby PATH environment file was not present: $env_file"
fi

echo "Open a new shell for the PATH change to take effect."
