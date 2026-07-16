#!/bin/bash
set -euo pipefail

# setup-goose.sh - Install Ferrosa Memory as a Goose extension
#
# This script:
# 1. Detects platform and architecture
# 2. Installs the ferrosa-memory binary to ~/.local/bin/
# 3. Optionally sets up the native service (launchd on macOS)
# 4. Adds a Goose extension entry to ~/.config/goose/config.yaml
#
# Usage: ./scripts/setup-goose.sh [--dry-run] [--force] [--skip-service]

DRY_RUN=false
FORCE=false
SKIP_SERVICE=false
VERSION="0.23.0"
REPO_OWNER="ferrosa-suite"
REPO_NAME="ferrosa-memory"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --force)
            FORCE=true
            shift
            ;;
        --skip-service)
            SKIP_SERVICE=true
            shift
            ;;
        --version)
            VERSION="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Detect platform
PLATFORM=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Map to GitHub release asset names
case "$PLATFORM" in
    linux)
        PLATFORM="linux"
        ;;
    darwin)
        PLATFORM="darwin"
        ;;
    *)
        echo "Error: Unsupported platform: $PLATFORM"
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64)
        ARCH="x86_64"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        ;;
    *)
        echo "Error: Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

BINARY_NAME="ferrosa-memory"
INSTALL_DIR="$HOME/.local/bin"
INSTALL_PATH="$INSTALL_DIR/$BINARY_NAME"
GOOSE_CONFIG="$HOME/.config/goose/config.yaml"

# Log function
log() {
    if [ "$DRY_RUN" = true ]; then
        echo "[dry-run] $*"
    else
        echo "$*"
    fi
}

# Check if binary is already installed
check_existing() {
    if [ -f "$INSTALL_PATH" ] && [ "$FORCE" = false ]; then
        echo "Binary already installed at: $INSTALL_PATH"
        echo "Use --force to overwrite or --dry-run to preview changes."
        exit 0
    fi
}

# Download pre-built binary
download_binary() {
    local url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/v${VERSION}/${BINARY_NAME}-${PLATFORM}-${ARCH}"
    
    log "Downloading ${BINARY_NAME} from: ${url}"
    
    if [ "$DRY_RUN" = false ]; then
        mkdir -p "$INSTALL_DIR"
        if curl -sL "$url" -o "$INSTALL_PATH"; then
            chmod +x "$INSTALL_PATH"
            log "Binary installed to: $INSTALL_PATH"
        else
            echo "Error: Failed to download binary"
            exit 1
        fi
    else
        log "Would download to: $INSTALL_PATH"
    fi
}

# Fallback to cargo install
cargo_install() {
    log "No pre-built binary found, falling back to cargo install..."
    
    if ! command -v cargo &>/dev/null; then
        echo "Error: cargo not found. Please install Rust from https://rustup.rs/"
        exit 1
    fi
    
    if [ "$DRY_RUN" = false ]; then
        # Build the MCP server binary specifically
        local build_dir
        build_dir=$(mktemp -d)
        cd "$build_dir"
        
        # Clone and build (simplified - in production you'd use a tagged release)
        log "Building from source..."
        cargo build --release -p ferrosa-memory-mcp --bin ferrosa-memory
        
        if [ -f "target/release/ferrosa-memory" ]; then
            mkdir -p "$INSTALL_DIR"
            cp "target/release/ferrosa-memory" "$INSTALL_PATH"
            chmod +x "$INSTALL_PATH"
            log "Binary built and installed to: $INSTALL_PATH"
        else
            echo "Error: Build failed"
            exit 1
        fi
        
        cd - > /dev/null
        rm -rf "$build_dir"
    else
        log "Would build and install via cargo"
    fi
}

# Set up native service (launchd on macOS)
setup_service() {
    if [ "$SKIP_SERVICE" = true ]; then
        log "Skipping native service setup (--skip-service)"
        return 0
    fi
    
    if [ "$PLATFORM" != "darwin" ]; then
        log "Native service setup is macOS-only. Skipping on $PLATFORM."
        return 0
    fi
    
    log "Setting up launchd service..."
    
    if [ "$DRY_RUN" = false ]; then
        # Use the existing setup.sh if available
        local repo_root
        repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
        
        if [ -f "$repo_root/setup.sh" ]; then
            log "Running existing setup.sh..."
            "$repo_root/setup.sh" --skip-build --no-verify 2>&1 | head -20 || true
        else
            log "setup.sh not found, skipping service setup"
        fi
    else
        log "Would run: $repo_root/setup.sh --skip-build --no-verify"
    fi
}

# Add Goose extension config
add_extension_config() {
    local ext_name="ferrosa-memory"
    local ext_cmd="$BINARY_NAME"
    
    log "Adding Goose extension config..."
    
    if [ "$DRY_RUN" = false ]; then
        mkdir -p "$(dirname "$GOOSE_CONFIG")"
        
        # Check if extension already exists
        if grep -q "^  ${ext_name}:" "$GOOSE_CONFIG" 2>/dev/null && [ "$FORCE" = false ]; then
            log "Extension '${ext_name}' already exists in config. Use --force to update."
            return 0
        fi
        
        # Create or update config
        if [ ! -f "$GOOSE_CONFIG" ]; then
            cat > "$GOOSE_CONFIG" <<EOF
extensions:
  ${ext_name}:
    name: Ferrosa Memory
    cmd: ${ext_cmd}
    enabled: true
    type: stdio
    description: Durable working memory for AI agents
    timeout: 300
EOF
        else
            # Append extension to existing config
            if ! grep -q "^extensions:" "$GOOSE_CONFIG"; then
                echo "extensions:" >> "$GOOSE_CONFIG"
            fi
            
            cat >> "$GOOSE_CONFIG" <<EOF
  ${ext_name}:
    name: Ferrosa Memory
    cmd: ${ext_cmd}
    enabled: true
    type: stdio
    description: Durable working memory for AI agents
    timeout: 300
EOF
        fi
        
        log "Extension config added to: $GOOSE_CONFIG"
    else
        log "Would add extension config to: $GOOSE_CONFIG"
    fi
}

# Main flow
echo "Setting up Ferrosa Memory as a Goose extension..."
echo "Platform: ${PLATFORM}-${ARCH}"
echo "Version: ${VERSION}"
echo ""

check_existing
download_binary
cargo_install
setup_service
add_extension_config

echo ""
echo "Setup complete!"
if [ "$DRY_RUN" = false ]; then
    echo ""
    echo "Next steps:"
    if [ "$SKIP_SERVICE" = true ]; then
        echo "  1. Start the Ferrosa Memory service manually, or run: ./setup.sh"
    else
        echo "  1. Start the Ferrosa Memory service (if not already running)"
    fi
    echo "  2. Restart Goose to activate the extension"
    echo "  3. Verify with: goose configure"
else
    echo ""
    echo "Dry run complete. No changes were made."
fi
