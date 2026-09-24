#!/bin/bash
set -euo pipefail

# Build native-devtools-mcp from source.
# For users who want to verify the code before trusting the binary.

GREEN='\033[0;32m'
RED='\033[0;31m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

echo ""
echo -e "${BOLD}native-devtools-mcp — Build from Source${NC}"
echo ""

# Check prerequisites
check_prereq() {
    if ! command -v "$1" &>/dev/null; then
        echo -e "  ${RED}✗${NC} $1 is not installed."
        echo "    $2"
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} $1 found"
}

echo -e "${BOLD}Checking prerequisites...${NC}"
echo ""
check_prereq "git" "Install from https://git-scm.com/"
check_prereq "cargo" "Install Rust from https://rustup.rs/"
echo ""

# Determine if we're already in the repo
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)"
if [[ -f "$SCRIPT_DIR/../Cargo.toml" ]] && grep -q 'name = "native-devtools-mcp"' "$SCRIPT_DIR/../Cargo.toml" 2>/dev/null; then
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    echo -e "  Using existing repo at: ${DIM}${PROJECT_ROOT}${NC}"
else
    # Clone the repo
    CLONE_DIR="${1:-native-devtools-mcp}"
    if [[ -d "$CLONE_DIR" ]]; then
        PROJECT_ROOT="$(cd "$CLONE_DIR" && pwd)"
        echo -e "  Using existing directory: ${DIM}${PROJECT_ROOT}${NC}"
    else
        echo "  Cloning repository..."
        git clone https://github.com/vectora-foundry/native-devtools-mcp.git "$CLONE_DIR"
        PROJECT_ROOT="$(cd "$CLONE_DIR" && pwd)"
    fi
fi
echo ""

# Optional: review source
echo -n "  Open source code for review before building? [y/N] "
read -r review_answer
if [[ "${review_answer}" =~ ^[Yy]$ ]]; then
    if [[ "$(uname)" == "Darwin" ]]; then
        open "$PROJECT_ROOT"
    elif command -v xdg-open &>/dev/null; then
        xdg-open "$PROJECT_ROOT"
    elif command -v explorer.exe &>/dev/null; then
        explorer.exe "$PROJECT_ROOT"
    fi
    echo ""
    echo -n "  Press Enter when ready to build..."
    read -r
fi
echo ""

# Build
echo -e "${BOLD}Building release binary...${NC}"
echo ""
cd "$PROJECT_ROOT"
cargo build --release
echo ""

# Find the binary
BINARY="$PROJECT_ROOT/target/release/native-devtools-mcp"
OS_TYPE="$(uname -o 2>/dev/null || true)"
if [[ "$OS_TYPE" == "Msys" ]] || [[ "$OS_TYPE" == "Cygwin" ]]; then
    BINARY="${BINARY}.exe"
fi

if [[ ! -f "$BINARY" ]]; then
    echo -e "  ${RED}✗${NC} Binary not found at expected path"
    exit 1
fi

# Hash the binary
HASH=$(shasum -a 256 "$BINARY" | awk '{print $1}')

echo -e "  ${GREEN}✓${NC} Build complete!"
echo ""
echo -e "  Binary:  ${DIM}${BINARY}${NC}"
echo -e "  SHA-256: ${DIM}${HASH}${NC}"
echo ""

# Offer to run setup
echo -n "  Run setup now? [Y/n] "
read -r setup_answer
if [[ ! "${setup_answer}" =~ ^[Nn]$ ]]; then
    echo ""
    "$BINARY" setup
fi
