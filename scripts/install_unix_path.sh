#!/bin/sh
set -eu

dry_run=0
profile_path="${LULLABY_PROFILE:-${HOME:-}/.profile}"
config_home="${XDG_CONFIG_HOME:-${HOME:-}/.config}"
env_dir="$config_home/lullaby"
env_file="$env_dir/env"

usage() {
    echo "usage: ./install.sh [--dry-run] [--profile path]" >&2
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
    echo "HOME is not set; cannot install a user PATH helper." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
package_root=$script_dir
bin_path="$package_root/bin"
lullaby_bin="$bin_path/lullaby"

if [ ! -f "$lullaby_bin" ]; then
    echo "lullaby not found at $lullaby_bin. Run this script from the root of the Lullaby portable package." >&2
    exit 1
fi

source_line=". \"$env_file\""
env_contents="# Lullaby portable package PATH
export PATH=\"$bin_path:\$PATH\"
"

if [ "$dry_run" -eq 1 ]; then
    echo "dry-run: would write PATH environment file: $env_file"
    echo "dry-run: would ensure profile sources it: $profile_path"
    echo "dry-run: would add to PATH: $bin_path"
    exit 0
fi

mkdir -p "$env_dir"
printf "%s" "$env_contents" > "$env_file"
chmod 600 "$env_file"

if [ ! -f "$profile_path" ]; then
    : > "$profile_path"
fi

if grep -Fx "$source_line" "$profile_path" >/dev/null 2>&1; then
    echo "Lullaby PATH environment file is already sourced by: $profile_path"
else
    {
        printf "\n# Lullaby portable package PATH\n"
        printf "%s\n" "$source_line"
    } >> "$profile_path"
    echo "Updated shell profile: $profile_path"
fi

echo "Added Lullaby to the user PATH through: $env_file"
echo "Open a new shell, then run: lullaby --version"
