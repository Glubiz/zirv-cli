#!/usr/bin/env sh
set -eu

REPO="Glubiz/zirv-cli"
INSTALL_DIR="${ZIRV_INSTALL_DIR:-/usr/local/bin}"
BINARY_NAME="zirv"

normalize_version() {
    version="$1"
    # Strip leading 'v' if present
    echo "$version" | sed 's/^v//'
}

check_existing_install() {
    install_dir="$1"
    force="${ZIRV_INSTALL_FORCE:-0}"
    target_file="${install_dir}/zirv"

    # Only refuse if the exact file we're about to overwrite is package-manager-managed.
    # If the file doesn't exist, there's nothing to refuse — proceeding is fine.
    if [ ! -e "$target_file" ]; then
        return 0
    fi

    # File exists at the target location. Resolve it and check if it's package-managed.
    resolved="$target_file"
    if [ -L "$target_file" ]; then
        resolved=$(readlink -f "$target_file" 2>/dev/null || true)
    fi

    # Check if the resolved path is in a package-manager directory
    case "$resolved" in
        */Cellar/*|/usr/local/Cellar*|/opt/homebrew*|/home/linuxbrew/.linuxbrew*)
            if [ "$force" != "1" ]; then
                echo "Error: ${target_file} is a Homebrew-managed installation." >&2
                echo "Use 'brew upgrade zirv' to update, or set ZIRV_INSTALL_FORCE=1 to override." >&2
                exit 1
            fi
            ;;
    esac
}

get_latest_version() {
    curl -sSf "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"v([^"]+)".*/\1/'
}

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)

    case "$OS" in
        linux*)
            case "$ARCH" in
                x86_64|amd64) ;;
                *)
                    echo "Error: Unsupported architecture for Linux: ${ARCH}." >&2
                    echo "The prebuilt Linux release is x86_64-only. Build from source instead:" >&2
                    echo "  cargo install --git https://github.com/Glubiz/zirv-cli" >&2
                    exit 1
                    ;;
            esac
            echo "linux"
            ;;
        darwin*)
            case "$ARCH" in
                x86_64|amd64|arm64|aarch64) ;;
                *)
                    echo "Error: Unsupported architecture for macOS: ${ARCH}." >&2
                    exit 1
                    ;;
            esac
            echo "macos"
            ;;
        *)
            echo "Error: Unsupported operating system: $OS" >&2
            exit 1
            ;;
    esac
}

verify_checksum() {
    tmpdir="$1"
    archive="$2"
    url="$3"

    if command -v sha256sum >/dev/null 2>&1; then
        checker="sha256sum -c"
    elif command -v shasum >/dev/null 2>&1; then
        checker="shasum -a 256 -c"
    else
        # Fail closed: require a checksum tool to be available.
        # Omitting checksum verification is a security risk; require explicit opt-out only as last resort.
        echo "Error: neither sha256sum nor shasum is available." >&2
        echo "Checksum verification is required. Install one of:" >&2
        echo "  macOS: brew install coreutils (provides sha256sum)" >&2
        echo "  Linux: install gnu-coreutils or openssl package" >&2
        echo "Or set ZIRV_INSTALL_NO_CHECKSUM=1 to skip (not recommended)." >&2
        exit 1
    fi

    checksum_url="${url}.sha256"
    echo "Downloading ${checksum_url}..."
    if ! curl -sSfL -o "${tmpdir}/${archive}.sha256" "$checksum_url"; then
        echo "Error: this release does not publish a checksum for ${archive} (${checksum_url})." >&2
        echo "Refusing to install an unverified download. Pass a version that publishes one:" >&2
        echo "  https://github.com/${REPO}/releases" >&2
        exit 1
    fi

    if ! ( cd "$tmpdir" && $checker "${archive}.sha256" ); then
        echo "Error: checksum verification failed for ${archive}." >&2
        echo "The download may be corrupted or tampered with -- not installing it." >&2
        exit 1
    fi
    echo "Checksum verified."
}

main() {
    VERSION="${1:-$(get_latest_version)}"
    if [ -z "$VERSION" ]; then
        echo "Error: Could not determine latest version." >&2
        exit 1
    fi

    # Normalize version: strip leading 'v' if present
    VERSION=$(normalize_version "$VERSION")

    # Check for existing install before proceeding
    check_existing_install "$INSTALL_DIR"

    PLATFORM=$(detect_platform)
    ARCHIVE="${BINARY_NAME}-${VERSION}-${PLATFORM}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE}"

    echo "Installing zirv v${VERSION} for ${PLATFORM}..."

    # Use a private temp variable; do not overwrite the well-known TMPDIR
    zirv_tmpdir=$(mktemp -d)
    trap 'rm -rf "$zirv_tmpdir"' EXIT

    echo "Downloading ${URL}..."
    curl -sSfL -o "${zirv_tmpdir}/${ARCHIVE}" "$URL" || {
        echo "Error: Failed to download ${URL}" >&2
        echo "Check that v${VERSION} exists: https://github.com/${REPO}/releases" >&2
        exit 1
    }

    verify_checksum "$zirv_tmpdir" "$ARCHIVE" "$URL"

    echo "Extracting..."
    tar -xzf "${zirv_tmpdir}/${ARCHIVE}" -C "$zirv_tmpdir"
    chmod +x "${zirv_tmpdir}/${BINARY_NAME}"

    if [ ! -d "$INSTALL_DIR" ]; then
        echo "Creating ${INSTALL_DIR}..."
        mkdir -p "$INSTALL_DIR" 2>/dev/null || sudo mkdir -p "$INSTALL_DIR"
    fi

    if [ -w "$INSTALL_DIR" ]; then
        mv "${zirv_tmpdir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${zirv_tmpdir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    fi

    echo "zirv v${VERSION} installed to ${INSTALL_DIR}/${BINARY_NAME}"

    # Check for PATH shadowing
    installed_path="${INSTALL_DIR}/${BINARY_NAME}"
    found_path=$(command -v zirv 2>/dev/null || true)
    verify_cmd="zirv version"

    if [ -n "$found_path" ] && [ "$found_path" != "$installed_path" ]; then
        echo "WARNING: PATH shadowing detected!" >&2
        echo "  Installed to: $installed_path" >&2
        echo "  But found on PATH: $found_path" >&2
        echo "  Ensure ${INSTALL_DIR} comes before other zirv locations in your PATH." >&2
        # Use full path in verification hint since bare 'zirv' would run the shadowed copy
        verify_cmd="${installed_path} version"
    fi

    echo "Run '${verify_cmd}' to verify, then 'zirv setup' to configure."
}

main "$@"
