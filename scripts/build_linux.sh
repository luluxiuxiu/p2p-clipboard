#!/usr/bin/env bash
set -euo pipefail

# p2p-clipboard Linux 编译脚本 (在 WSL 中执行)
# 用法: bash scripts/build_linux.sh <项目WSL路径>

GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }

PROJECT_ROOT="${1:-.}"

# 检查并安装 Rust
if ! command -v cargo &>/dev/null; then
    if [ -f "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
    fi
fi

if ! command -v cargo &>/dev/null; then
    log_info "WSL 中未找到 Rust，正在安装..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env"
fi

log_info "Rust 版本: $(rustc --version)"

# 检查并安装编译依赖
install_build_deps() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
    fi
    local DISTRO_ID="${ID:-unknown}"

    case "$DISTRO_ID" in
        ubuntu|debian|linuxmint|pop)
            local NEEDED=""
            dpkg -s build-essential &>/dev/null || NEEDED="$NEEDED build-essential"
            dpkg -s pkg-config &>/dev/null || NEEDED="$NEEDED pkg-config"
            dpkg -s libxcb1-dev &>/dev/null || NEEDED="$NEEDED libxcb1-dev"
            dpkg -s libxcb-render0-dev &>/dev/null || NEEDED="$NEEDED libxcb-render0-dev"
            dpkg -s libxcb-shape0-dev &>/dev/null || NEEDED="$NEEDED libxcb-shape0-dev"
            dpkg -s libxcb-xfixes0-dev &>/dev/null || NEEDED="$NEEDED libxcb-xfixes0-dev"

            if [ -n "$NEEDED" ]; then
                log_info "安装编译依赖:$NEEDED"
                sudo apt-get update -qq
                sudo apt-get install -y -qq $NEEDED
            else
                log_info "编译依赖已就绪"
            fi
            ;;
        fedora|rocky|rhel|centos|almalinux)
            local PKG_MGR="dnf"
            command -v dnf &>/dev/null || PKG_MGR="yum"
            sudo $PKG_MGR install -y gcc gcc-c++ make pkgconfig libxcb-devel xcb-util-renderutil-devel
            ;;
        *)
            log_error "不支持的 WSL 发行版: $DISTRO_ID"
            log_error "请手动安装: build-essential pkg-config libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev"
            exit 1
            ;;
    esac
}

install_build_deps

cd "$PROJECT_ROOT"
log_info "开始编译 Linux 版本..."
cargo build --release

if [ ! -f "target/release/p2p-clipboard" ]; then
    log_error "编译产物不存在"
    exit 1
fi

log_info "Linux 编译完成: target/release/p2p-clipboard"
