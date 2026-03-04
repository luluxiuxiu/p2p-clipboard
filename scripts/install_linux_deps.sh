#!/usr/bin/env bash
set -euo pipefail

# p2p-clipboard Linux 安装脚本
# 支持: Ubuntu/Debian, Rocky/RHEL/CentOS/Fedora
# 用途: 安装运行时依赖库 + 安装 systemd 用户服务

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
        log_info "检测到: $PRETTY_NAME"
    else
        log_error "无法检测 Linux 发行版 (/etc/os-release 不存在)"
        exit 1
    fi
}

install_runtime_deps_debian() {
    log_info "安装 Debian/Ubuntu 运行时依赖..."
    sudo apt-get update -qq

    # arboard 运行时依赖 (X11 剪贴板)
    sudo apt-get install -y -qq \
        libxcb1 \
        libxcb-render0 \
        libxcb-shape0 \
        libxcb-xfixes0

    # Wayland 剪贴板支持 (可选)
    sudo apt-get install -y -qq \
        libwayland-client0 \
        2>/dev/null || log_warn "Wayland 库不可用，仅支持 X11"

    # 文件剪贴板工具: X11 用 xclip, Wayland 用 wl-clipboard
    sudo apt-get install -y -qq xclip 2>/dev/null || true
    sudo apt-get install -y -qq wl-clipboard 2>/dev/null || true

    log_info "Debian/Ubuntu 运行时依赖安装完成"
}

install_runtime_deps_rhel() {
    log_info "安装 RHEL/Rocky/CentOS/Fedora 运行时依赖..."

    local PKG_MGR="dnf"
    if ! command -v dnf &>/dev/null; then
        PKG_MGR="yum"
    fi

    # arboard 运行时依赖 (X11 剪贴板)
    sudo $PKG_MGR install -y \
        libxcb \
        xcb-util-renderutil

    # Wayland 剪贴板支持 (可选)
    sudo $PKG_MGR install -y \
        libwayland-client \
        2>/dev/null || log_warn "Wayland 库不可用，仅支持 X11"

    # 文件剪贴板工具
    sudo $PKG_MGR install -y xclip 2>/dev/null || true
    sudo $PKG_MGR install -y wl-clipboard 2>/dev/null || true

    log_info "RHEL/Rocky 运行时依赖安装完成"
}

install_service() {
    local binary_path="${1:-/usr/local/bin/p2p-clipboard}"

    if [ ! -f "$binary_path" ]; then
        log_warn "二进制文件不存在: $binary_path"
        log_warn "请先将编译好的程序复制到该路径: sudo cp p2p-clipboard $binary_path"
        return 1
    fi

    log_info "创建 systemd 用户服务..."

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

    systemctl --user daemon-reload
    systemctl --user enable p2p-clipboard
    systemctl --user start p2p-clipboard

    log_info "服务已安装并启动"
    log_info "查看状态: systemctl --user status p2p-clipboard"
    log_info "查看日志: journalctl --user -u p2p-clipboard -f"
}

main() {
    if [ $# -eq 0 ] || [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
        echo "用法: $0 [选项]"
        echo ""
        echo "选项:"
        echo "  --deps       安装运行时依赖库"
        echo "  --service    安装 systemd 用户服务 (可选: --service /path/to/binary)"
        echo "  --all        安装依赖 + 服务"
        echo "  --help       显示帮助"
        exit 0
    fi

    local do_deps=false
    local do_service=false
    local binary_path="/usr/local/bin/p2p-clipboard"

    for arg in "$@"; do
        case "$arg" in
            --deps)
                do_deps=true
                ;;
            --service)
                do_service=true
                ;;
            --all)
                do_deps=true
                do_service=true
                ;;
            /*)
                binary_path="$arg"
                ;;
            *)
                log_error "未知选项: $arg"
                exit 1
                ;;
        esac
    done

    if $do_deps; then
        detect_distro
        case "$DISTRO_ID" in
            ubuntu|debian|linuxmint|pop)
                install_runtime_deps_debian
                ;;
            rocky|rhel|centos|fedora|almalinux)
                install_runtime_deps_rhel
                ;;
            *)
                if echo "$DISTRO_LIKE" | grep -qi "debian\|ubuntu"; then
                    install_runtime_deps_debian
                elif echo "$DISTRO_LIKE" | grep -qi "rhel\|fedora\|centos"; then
                    install_runtime_deps_rhel
                else
                    log_error "不支持的发行版: $DISTRO_ID"
                    log_error "请手动安装: libxcb, xclip 或 wl-clipboard"
                    exit 1
                fi
                ;;
        esac
    fi

    if $do_service; then
        install_service "$binary_path"
    fi

    log_info "完成"
}

main "$@"
