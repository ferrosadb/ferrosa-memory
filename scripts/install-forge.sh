#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Install the Forge CLI (`frg`) from ferrosadb/forge.

Usage: scripts/install-forge.sh [options]

Options:
  --repo URL
      Forge git repository. Default: https://github.com/ferrosadb/forge.git.
  --dir PATH
      Local source checkout. Default: ~/.cache/ferrosa-memory/forge.
  --bin-dir PATH
      Directory where `frg` is installed. Default: ~/.local/bin.
  --help
      Show this help.
EOF
}

log() {
    printf '[install-forge] %s\n' "$*"
}

die() {
    printf '[install-forge] error: %s\n' "$*" >&2
    exit 1
}

forge_repo="${FERROSA_FORGE_REPO:-https://github.com/ferrosadb/forge.git}"
forge_dir="${FERROSA_FORGE_DIR:-${HOME}/.cache/ferrosa-memory/forge}"
forge_bin_dir="${FERROSA_FORGE_BIN_DIR:-${HOME}/.local/bin}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo)
            forge_repo="${2:-}"
            [[ -n "$forge_repo" ]] || die "--repo requires a value"
            shift 2
            ;;
        --dir)
            forge_dir="${2:-}"
            [[ -n "$forge_dir" ]] || die "--dir requires a value"
            shift 2
            ;;
        --bin-dir)
            forge_bin_dir="${2:-}"
            [[ -n "$forge_bin_dir" ]] || die "--bin-dir requires a value"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

command -v git >/dev/null 2>&1 || die "git is required to fetch Forge"
command -v cargo >/dev/null 2>&1 || die "cargo is required to build Forge"

mkdir -p "$(dirname "$forge_dir")" "$forge_bin_dir"

if [[ -d "$forge_dir/.git" ]]; then
    log "updating Forge checkout at $forge_dir"
    git -C "$forge_dir" fetch --prune origin
    git -C "$forge_dir" pull --ff-only
elif [[ -e "$forge_dir" ]]; then
    die "Forge dir exists but is not a git checkout: $forge_dir"
else
    log "cloning Forge from $forge_repo to $forge_dir"
    git clone --depth 1 "$forge_repo" "$forge_dir"
fi

log "building Forge CLI"
cargo build --release --manifest-path "$forge_dir/Cargo.toml"

frg_src="$forge_dir/target/release/frg"
[[ -x "$frg_src" ]] || die "Forge build did not produce executable: $frg_src"

install -m 0755 "$frg_src" "$forge_bin_dir/frg"
log "installed $forge_bin_dir/frg"

"$forge_bin_dir/frg" --version >/dev/null
log "Forge CLI is ready"
