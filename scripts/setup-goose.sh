#!/usr/bin/env bash
set -euo pipefail

# setup-goose.sh - Install Ferrosa Memory as a Goose extension
#
# This script:
#   1. Detects platform and architecture
#   2. Installs the ferrosa-memory-mcp binary to ~/.local/bin/
#      (downloading a signed release tarball, or building from this checkout)
#   3. Optionally sets up the native service (launchd on macOS) via ./setup.sh
#   4. Adds a Goose stdio extension entry to ~/.config/goose/config.yaml,
#      editing the YAML in place without duplicating or clobbering other config
#
# Usage: ./scripts/setup-goose.sh [--dry-run] [--force] [--skip-service] [--version VER]

usage() {
    cat <<'EOF'
Install Ferrosa Memory as a Goose extension.

Usage: ./scripts/setup-goose.sh [options]

Options:
  --dry-run        Print every action that WOULD be taken; make no changes.
  --force          Reinstall the binary and replace an existing Goose entry.
  --skip-service   Do not install/restart the native (launchd) service.
  --version VER    Release version to install (default: the pinned default).
  --help, -h       Show this help.
EOF
}

log() {
    if [[ "${DRY_RUN:-false}" == true ]]; then
        printf '[setup-goose][dry-run] %s\n' "$*"
    else
        printf '[setup-goose] %s\n' "$*"
    fi
}

die() {
    printf '[setup-goose] error: %s\n' "$*" >&2
    exit 1
}

DRY_RUN=false
FORCE=false
SKIP_SERVICE=false
VERSION="0.23.0"
REPO_OWNER="ferrosadb"
REPO_NAME="ferrosa-memory"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=true; shift ;;
        --force) FORCE=true; shift ;;
        --skip-service) SKIP_SERVICE=true; shift ;;
        --version)
            VERSION="${2:-}"
            [[ -n "$VERSION" ]] || die "--version requires a value"
            shift 2
            ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# --- Resolve paths and platform up front (safe in dry-run: pure computation) ---

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$PLATFORM" in
    linux) PLATFORM="linux" ;;
    darwin) PLATFORM="darwin" ;;
    *) die "unsupported platform: $PLATFORM" ;;
esac

case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) die "unsupported architecture: $ARCH" ;;
esac

# Release assets are named with Rust target triples. macOS ships
# Apple-Silicon-only by design (no x86_64-apple-darwin asset exists).
target_triple() {
    case "${PLATFORM}-${ARCH}" in
        darwin-aarch64) echo "aarch64-apple-darwin" ;;
        linux-aarch64)  echo "aarch64-unknown-linux-musl" ;;
        linux-x86_64)   echo "x86_64-unknown-linux-musl" ;;
        darwin-x86_64)  die "Intel macOS is not supported; ferrosa-memory is Apple-Silicon-only on macOS. Build from source on this host or use an arm64 Mac." ;;
        *) die "no prebuilt binary for ${PLATFORM}-${ARCH}" ;;
    esac
}

TRIPLE="$(target_triple)"
BINARY_NAME="ferrosa-memory-mcp"
INSTALL_DIR="$HOME/.local/bin"
INSTALL_PATH="$INSTALL_DIR/$BINARY_NAME"
GOOSE_CONFIG="$HOME/.config/goose/config.yaml"

ASSET="ferrosa-memory-v${VERSION}-${TRIPLE}.tar.gz"
BASE_URL="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/v${VERSION}"
ASSET_URL="${BASE_URL}/${ASSET}"
SUMS_URL="${BASE_URL}/SHA256SUMS"

# --- Helpers ---

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        return 1
    fi
}

# Verify a file is a real native executable, not an HTML error page or text.
assert_is_executable_binary() {
    local path="$1" desc
    [[ -s "$path" ]] || die "expected binary is missing or empty: $path"
    desc="$(file -b "$path" 2>/dev/null || echo unknown)"
    case "$desc" in
        *Mach-O*|*ELF*|*executable*) : ;;
        *) die "not an executable binary ($desc): $path" ;;
    esac
}

# Download + verify + extract the release tarball. Returns non-zero (without
# dying) when the artifact simply isn't available, so the caller can fall back
# to a source build. Integrity failures (checksum/corruption) die loudly.
download_binary() {
    log "would install ${BINARY_NAME} ${VERSION} (${TRIPLE}) to ${INSTALL_PATH}"
    log "  source: ${ASSET_URL}"

    if [[ "$DRY_RUN" == true ]]; then
        return 0
    fi

    local workdir archive
    workdir="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$workdir'" RETURN
    archive="${workdir}/${ASSET}"

    log "downloading ${ASSET}"
    if ! curl -fsSL "$ASSET_URL" -o "$archive"; then
        log "no prebuilt asset at ${ASSET_URL}"
        return 1
    fi

    # Verify checksum against the release SHA256SUMS. Present == must match.
    if curl -fsSL "$SUMS_URL" -o "${workdir}/SHA256SUMS"; then
        local expected actual
        expected="$(awk -v f="$ASSET" '$2 == f {print $1}' "${workdir}/SHA256SUMS")"
        if [[ -z "$expected" ]]; then
            die "SHA256SUMS has no entry for ${ASSET}; refusing to install unverified artifact"
        fi
        actual="$(sha256_of "$archive")" || die "no sha256 tool (sha256sum/shasum) available to verify download"
        [[ "$actual" == "$expected" ]] || die "checksum mismatch for ${ASSET}: expected ${expected}, got ${actual}"
        log "checksum verified"
    else
        # Disclosed fallback: older releases may predate SHA256SUMS. We still
        # validate the artifact is a real binary below, but say so loudly.
        log "WARNING: SHA256SUMS not found at ${SUMS_URL}; proceeding without checksum verification"
    fi

    # A valid gzip tarball must list cleanly; a stray HTML page will not.
    tar -tzf "$archive" >/dev/null 2>&1 || die "downloaded file is not a valid tar.gz: ${archive}"
    tar -xzf "$archive" -C "$workdir" || die "failed to extract ${archive}"

    local extracted="${workdir}/${BINARY_NAME}"
    [[ -f "$extracted" ]] || die "release tarball did not contain ${BINARY_NAME}"
    assert_is_executable_binary "$extracted"

    mkdir -p "$INSTALL_DIR"
    install -m 0755 "$extracted" "$INSTALL_PATH"
    log "installed ${BINARY_NAME} to ${INSTALL_PATH}"
    return 0
}

# Real fallback: build ferrosa-memory-mcp from THIS checkout (the script lives
# inside the repo). Only invoked when the prebuilt download is unavailable.
cargo_install() {
    log "falling back to building ${BINARY_NAME} from source at ${REPO_ROOT}"

    [[ -f "${REPO_ROOT}/Cargo.toml" ]] || die "cannot build from source: no Cargo.toml at ${REPO_ROOT}. Clone ${REPO_OWNER}/${REPO_NAME} and run this script from inside the checkout."
    command -v cargo >/dev/null 2>&1 || die "cargo not found and no prebuilt binary available. Install Rust from https://rustup.rs/ or run on a supported platform."

    if [[ "$DRY_RUN" == true ]]; then
        log "would run: (cd ${REPO_ROOT} && cargo build --release --package ferrosa-memory-mcp)"
        log "would install ${REPO_ROOT}/target/release/${BINARY_NAME} to ${INSTALL_PATH}"
        return 0
    fi

    ( cd "$REPO_ROOT" && cargo build --release --package ferrosa-memory-mcp ) \
        || die "cargo build failed for ferrosa-memory-mcp"

    local built="${REPO_ROOT}/target/release/${BINARY_NAME}"
    [[ -f "$built" ]] || die "build succeeded but ${built} is missing"
    assert_is_executable_binary "$built"

    mkdir -p "$INSTALL_DIR"
    install -m 0755 "$built" "$INSTALL_PATH"
    log "built and installed ${BINARY_NAME} to ${INSTALL_PATH}"
}

install_binary() {
    if [[ -f "$INSTALL_PATH" && "$FORCE" == false ]]; then
        log "${BINARY_NAME} already present at ${INSTALL_PATH}; skipping install (use --force to reinstall)"
        return 0
    fi
    if ! download_binary; then
        log "prebuilt binary unavailable; using source build"
        cargo_install
    fi
}

# Native service setup delegates to the repo's setup.sh (macOS launchd).
setup_service() {
    if [[ "$SKIP_SERVICE" == true ]]; then
        log "skipping native service setup (--skip-service)"
        return 0
    fi
    if [[ "$PLATFORM" != "darwin" ]]; then
        log "native service auto-install is macOS-only; skipping on ${PLATFORM}"
        return 0
    fi

    local setup_sh="${REPO_ROOT}/setup.sh"
    if [[ ! -f "$setup_sh" ]]; then
        log "no setup.sh at ${REPO_ROOT}; skipping native service setup"
        return 0
    fi

    if [[ "$DRY_RUN" == true ]]; then
        log "would run: ${setup_sh} --skip-build --no-verify"
        return 0
    fi

    log "running ${setup_sh} --skip-build --no-verify"
    # Fail loud: no output truncation, no error swallowing.
    "$setup_sh" --skip-build --no-verify \
        || die "native service setup failed (${setup_sh} --skip-build --no-verify)"
}

# Add/replace the Goose stdio extension entry, editing YAML in place.
add_extension_config() {
    local ext_name="ferrosa-memory"

    if [[ "$DRY_RUN" != true ]]; then
        mkdir -p "$(dirname "$GOOSE_CONFIG")"
        if [[ -f "$GOOSE_CONFIG" ]]; then
            cp "$GOOSE_CONFIG" "${GOOSE_CONFIG}.bak.$(date +%Y%m%d%H%M%S)"
        fi
    fi

    command -v python3 >/dev/null 2>&1 || die "python3 is required to edit ${GOOSE_CONFIG} safely"

    local ext_json
    ext_json="$(cat <<JSON
{
  "name": "${ext_name}",
  "type": "stdio",
  "cmd": "${INSTALL_PATH}",
  "args": [],
  "envs": {},
  "enabled": true,
  "timeout": 300,
  "description": "Durable working memory for AI agents"
}
JSON
)"

    GOOSE_CONFIG="$GOOSE_CONFIG" EXT_NAME="$ext_name" EXT_JSON="$ext_json" \
    FORCE="$FORCE" DRY_RUN="$DRY_RUN" python3 - <<'PY'
import json, os, sys

path = os.environ["GOOSE_CONFIG"]
name = os.environ["EXT_NAME"]
entry = json.loads(os.environ["EXT_JSON"])
force = os.environ["FORCE"] == "true"
dry_run = os.environ["DRY_RUN"] == "true"

def note(msg):
    prefix = "[setup-goose][dry-run]" if dry_run else "[setup-goose]"
    print(f"{prefix} {msg}")

def die(msg):
    print(f"[setup-goose] error: {msg}", file=sys.stderr)
    raise SystemExit(1)

existing_text = ""
if os.path.exists(path):
    with open(path, "r") as fh:
        existing_text = fh.read()

try:
    import yaml
    have_yaml = True
except ImportError:
    have_yaml = False

def render_entry_lines(name, entry):
    # Deterministic 2-space YAML for the "  <name>:" block (4-space fields).
    lines = [f"  {name}:"]
    for key, val in entry.items():
        if isinstance(val, bool):
            rendered = "true" if val else "false"
        elif isinstance(val, list) and not val:
            rendered = "[]"
        elif isinstance(val, dict) and not val:
            rendered = "{}"
        elif isinstance(val, int):
            rendered = str(val)
        else:
            rendered = json.dumps(str(val))  # safe-quoted scalar
        lines.append(f"    {key}: {rendered}")
    return lines

if have_yaml:
    data = yaml.safe_load(existing_text) if existing_text.strip() else {}
    if data is None:
        data = {}
    if not isinstance(data, dict):
        die(f"{path} is not a YAML mapping; refusing to edit")
    extensions = data.get("extensions")
    if extensions is None:
        extensions = {}
        data["extensions"] = extensions
    if not isinstance(extensions, dict):
        die(f"'extensions' in {path} is not a mapping; refusing to edit")

    present = name in extensions
    verb, done = ("replace", "replaced") if present else ("add", "added")
    if present and not force:
        note(f"extension '{name}' already present in {path}; use --force to replace")
        raise SystemExit(0)

    if dry_run:
        note(f"would {verb} extension '{name}' in {path} (yaml-aware)")
        raise SystemExit(0)

    extensions[name] = entry
    with open(path, "w") as fh:
        yaml.safe_dump(data, fh, default_flow_style=False, sort_keys=False)
    note(f"{done} extension '{name}' in {path}")
    raise SystemExit(0)

# --- Fallback: no PyYAML. Careful line-based, indentation-aware edit. ---
lines = existing_text.splitlines()
block = render_entry_lines(name, entry)

def find_top_key(lines, key):
    for i, ln in enumerate(lines):
        if ln.rstrip() == f"{key}:" or ln.startswith(f"{key}:"):
            if not ln.startswith((" ", "\t")):
                return i
    return -1

ext_idx = find_top_key(lines, "extensions")

if ext_idx == -1:
    if dry_run:
        note(f"would add extension '{name}' and a new 'extensions:' block to {path}")
        raise SystemExit(0)
    if lines and lines[-1].strip() != "":
        lines.append("")
    lines.append("extensions:")
    lines.extend(block)
else:
    # Scan the extensions block for an existing "  <name>:" child.
    start = -1
    i = ext_idx + 1
    while i < len(lines):
        ln = lines[i]
        if ln.strip() == "" or ln.lstrip().startswith("#"):
            i += 1
            continue
        indent = len(ln) - len(ln.lstrip(" "))
        if indent == 0:
            break  # left the extensions block
        if indent == 2 and ln.strip().rstrip(":") == name and ln.rstrip().endswith(":"):
            start = i
            break
        i += 1

    if start != -1 and not force:
        note(f"extension '{name}' already present in {path}; use --force to replace")
        raise SystemExit(0)

    if dry_run:
        verb = "replace" if start != -1 else "add"
        note(f"would {verb} extension '{name}' in {path} (line-based)")
        raise SystemExit(0)

    if start != -1:
        # Delete the existing entry's sub-block (everything indented > 2).
        end = start + 1
        while end < len(lines):
            ln = lines[end]
            if ln.strip() == "":
                end += 1
                continue
            indent = len(ln) - len(ln.lstrip(" "))
            if indent <= 2:
                break
            end += 1
        lines[start:end] = block
    else:
        lines[ext_idx + 1:ext_idx + 1] = block

with open(path, "w") as fh:
    fh.write("\n".join(lines) + "\n")
note(f"updated extension '{name}' in {path}")
PY
}

# Final verification: fail loud unless the binary and config entry are real.
verify_install() {
    if [[ "$DRY_RUN" == true ]]; then
        log "dry-run: skipping post-install verification"
        return 0
    fi
    assert_is_executable_binary "$INSTALL_PATH"
    log "verified binary: ${INSTALL_PATH}"
    grep -Eq "^[[:space:]]{2}ferrosa-memory:" "$GOOSE_CONFIG" \
        || die "Goose config ${GOOSE_CONFIG} is missing the ferrosa-memory extension entry after edit"
    log "verified Goose extension entry in ${GOOSE_CONFIG}"
}

# --- Main ---

echo "Setting up Ferrosa Memory as a Goose extension"
echo "  platform: ${PLATFORM}-${ARCH} (${TRIPLE})"
echo "  version:  ${VERSION}"
echo "  binary:   ${INSTALL_PATH}"
echo "  config:   ${GOOSE_CONFIG}"
echo ""

install_binary
setup_service
add_extension_config
verify_install

echo ""
if [[ "$DRY_RUN" == true ]]; then
    echo "Dry run complete. No changes were made."
else
    echo "Setup complete."
    echo ""
    echo "Next steps:"
    echo "  1. Point the extension at your Ferrosa instance by setting"
    echo "     FERROSA_MEMORY_CONFIG (in the extension 'envs' or your shell)."
    if [[ "$SKIP_SERVICE" == true ]]; then
        echo "  2. Start the Ferrosa Memory service, e.g. run: ./setup.sh"
    else
        echo "  2. Confirm the Ferrosa Memory service is running."
    fi
    echo "  3. Restart Goose, then verify with: goose configure"
fi
