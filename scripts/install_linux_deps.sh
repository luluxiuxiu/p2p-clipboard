#!/usr/bin/env bash
set -euo pipefail

# p2p-clipboard Linux 依赖安装脚本
# 支持: Ubuntu/Debian, Rocky/RHEL/CentOS/Fedora
# 用途: 安装编译和运行 p2p-clipboard 所需的全部系统依赖

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }

detect_distro() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        DISTRO_ID="${ID:-unknown}"
        DISTRO_LIKE="${ID_LIKE:-$DISTRO_ID}"
        DISTRO_VERSION="${VERSION_ID:-0}"
        log_info "Detected: $PRETTY_NAME"
    else
        log_error "Cannot detect Linux distribution (/etc/os-release not found)"
        exit 1
    fi
}

install_rust() {
    if command -v rustc &>/dev/null; then
        local rust_ver
        rust_ver=$(rustc --version)
        log_info "Rust already installed: $rust_ver"
    else
        log_info "Installing Rust via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
        log_info "Rust installed: $(rustc --version)"
    fi
}

install_deps_debian() {
    log_info "Installing dependencies for Debian/Ubuntu..."
    sudo apt-get update -qq

    # 编译工具链
    sudo apt-get install -y -qq \
        build-essential \
        pkg-config \
        curl

    # arboard image-data 依赖 (X11 clipboard + image processing)
    sudo apt-get install -y -qq \
        libxcb1-dev \
        libxcb-render0-dev \
        libxcb-shape0-dev \
        libxcb-xfixes0-dev

    # Wayland data-control 支持 (arboard wayland-data-control feature)
    sudo apt-get install -y -qq \
        libwayland-dev \
        libwayland-client0 \
        wayland-protocols \
        2>/dev/null || log_warn "Wayland packages not available, X11 only"

    # xclip/xsel 作为 fallback clipboard 工具
    sudo apt-get install -y -qq xclip 2>/dev/null || true

    log_info "Debian/Ubuntu dependencies installed"
}

install_deps_rhel() {
    log_info "Installing dependencies for RHEL/Rocky/CentOS/Fedora..."

    # 判断包管理器
    local PKG_MGR="dnf"
    if ! command -v dnf &>/dev/null; then
        PKG_MGR="yum"
    fi

    # 启用 EPEL (Rocky/RHEL 需要)
    if [[ "$DISTRO_ID" == "rocky" || "$DISTRO_ID" == "rhel" || "$DISTRO_ID" == "centos" ]]; then
        sudo $PKG_MGR install -y epel-release 2>/dev/null || log_warn "EPEL may already be enabled"
        # Rocky 9 / RHEL 9 需要启用 CRB
        if [[ "${DISTRO_VERSION%%.*}" -ge 9 ]]; then
            sudo $PKG_MGR config-manager --set-enabled crb 2>/dev/null || \
            sudo $PKG_MGR config-manager --set-enabled powertools 2>/dev/null || \
            log_warn "Could not enable CRB/PowerTools repo"
        fi
    fi

    # 编译工具链
    sudo $PKG_MGR groupinstall -y "Development Tools" 2>/dev/null || \
        sudo $PKG_MGR install -y gcc gcc-c++ make
    sudo $PKG_MGR install -y \
        pkgconfig \
        curl

    # arboard image-data 依赖 (X11)
    sudo $PKG_MGR install -y \
        libxcb-devel \
        xcb-util-renderutil-devel \
        xcb-util-devel

    # Wayland 支持
    sudo $PKG_MGR install -y \
        wayland-devel \
        wayland-protocols-devel \
        2>/dev/null || log_warn "Wayland packages not available, X11 only"

    # xclip fallback
    sudo $PKG_MGR install -y xclip 2>/dev/null || true

    log_info "RHEL/Rocky dependencies installed"
}

build_project() {
    log_info "Building p2p-clipboard..."
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    local project_dir
    project_dir="$(dirname "$script_dir")"

    cd "$project_dir"

    if ! command -v cargo &>/dev/null; then
        log_error "cargo not found. Please run: source ~/.cargo/env"
        exit 1
    fi

    cargo build --release

    local binary="$project_dir/target/release/p2p-clipboard"
    if [ -f "$binary" ]; then
        log_info "Build successful: $binary"
        log_info "You can copy it to /usr/local/bin:"
        log_info "  sudo cp $binary /usr/local/bin/"
    else
        log_error "Build failed, binary not found"
        exit 1
    fi
}

create_systemd_service() {
    local binary_path="${1:-/usr/local/bin/p2p-clipboard}"
    local service_file="/etc/systemd/system/p2p-clipboard.service"

    if [ ! -f "$binary_path" ]; then
        log_warn "Binary not found at $binary_path, skipping systemd service creation"
        log_warn "After building, run: sudo cp target/release/p2p-clipboard /usr/local/bin/"
        return
    fi

    log_info "Creating systemd user service..."

    local user_service_dir="$HOME/.config/systemd/user"
    mkdir -p "$user_service_dir"

    cat > "$user_service_dir/p2p-clipboard.service" << EOF
[Unit]
Description=P2P Clipboard Sync
After=graphical-session.target
Wants=graphical-session.target

[Service]
Type=simple
ExecStart=$binary_path
Restart=on-failure
RestartSec=5
Environment=DISPLAY=:0
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
EOF

    log_info "Systemd user service created at: $user_service_dir/p2p-clipboard.service"
    log_info "To enable and start:"
    log_info "  systemctl --user daemon-reload"
    log_info "  systemctl --user enable p2p-clipboard"
    log_info "  systemctl --user start p2p-clipboard"
    log_info "  systemctl --user status p2p-clipboard"
}

print_usage() {
    echo ""
    log_info "=== p2p-clipboard Linux Setup ==="
    echo ""
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --deps-only     Only install system dependencies (no Rust, no build)"
    echo "  --build         Install deps + Rust + build the project"
    echo "  --service       Also create a systemd user service"
    echo "  --all           Do everything: deps + Rust + build + service"
    echo "  --help          Show this help"
    echo ""
    echo "Examples:"
    echo "  $0 --all                    # Full setup"
    echo "  $0 --deps-only              # Just install system libs"
    echo "  $0 --build                  # Install deps and build"
    echo ""
}

main() {
    local do_deps=false
    local do_rust=false
    local do_build=false
    local do_service=false

    if [ $# -eq 0 ]; then
        print_usage
        exit 0
    fi

    for arg in "$@"; do
        case "$arg" in
            --deps-only)
                do_deps=true
                ;;
            --build)
                do_deps=true
                do_rust=true
                do_build=true
                ;;
            --service)
                do_service=true
                ;;
            --all)
                do_deps=true
                do_rust=true
                do_build=true
                do_service=true
                ;;
            --help|-h)
                print_usage
                exit 0
                ;;
            *)
                log_error "Unknown option: $arg"
                print_usage
                exit 1
                ;;
        esac
    done

    detect_distro

    if $do_deps; then
        case "$DISTRO_ID" in
            ubuntu|debian|linuxmint|pop)
                install_deps_debian
                ;;
            rocky|rhel|centos|fedora|almalinux)
                install_deps_rhel
                ;;
            *)
                # 尝试通过 ID_LIKE 判断
                if echo "$DISTRO_LIKE" | grep -qi "debian\|ubuntu"; then
                    install_deps_debian
                elif echo "$DISTRO_LIKE" | grep -qi "rhel\|fedora\|centos"; then
                    install_deps_rhel
                else
                    log_error "Unsupported distribution: $DISTRO_ID ($DISTRO_LIKE)"
                    log_error "Please manually install: libxcb-dev, pkg-config, build-essential equivalents"
                    exit 1
                fi
                ;;
        esac
    fi

    if $do_rust; then
        install_rust
    fi

    if $do_build; then
        build_project
    fi

    if $do_service; then
        create_systemd_service "/usr/local/bin/p2p-clipboard"
    fi

    echo ""
    log_info "Done! File receive directory on Linux: /tmp"
    log_info "Run p2p-clipboard with: p2p-clipboard [OPTIONS]"
    log_info "See: p2p-clipboard --help"
}

main "$@"
