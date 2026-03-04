# p2p-clipboard 构建脚本
# 在 Windows 上编译 Windows 版本，通过 WSL 编译 Linux 版本
# 用法: powershell -ExecutionPolicy Bypass -File scripts/build.ps1 [-Windows] [-Linux] [-All]

param(
    [switch]$Windows,
    [switch]$Linux,
    [switch]$All
)

$ErrorActionPreference = "Stop"

$ProjectRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$OutputDir = Join-Path $ProjectRoot "target" "dist"
$Version = "0.2.0"

function Write-Info  { param($msg) Write-Host "[INFO] $msg" -ForegroundColor Green }
function Write-Warn  { param($msg) Write-Host "[WARN] $msg" -ForegroundColor Yellow }
function Write-Err   { param($msg) Write-Host "[ERROR] $msg" -ForegroundColor Red }

function Test-Command {
    param($cmd)
    $null = Get-Command $cmd -ErrorAction SilentlyContinue
    return $?
}

function Build-Windows {
    Write-Info "===== 编译 Windows x86_64 版本 ====="

    if (-not (Test-Command "cargo")) {
        Write-Err "未找到 cargo，请先安装 Rust: https://rustup.rs"
        exit 1
    }

    Write-Info "Rust 版本: $(rustc --version)"

    Push-Location $ProjectRoot
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) {
            Write-Err "Windows 编译失败"
            exit 1
        }

        $WinBinary = Join-Path $ProjectRoot "target" "release" "p2p-clipboard.exe"
        if (-not (Test-Path $WinBinary)) {
            Write-Err "编译产物不存在: $WinBinary"
            exit 1
        }

        $WinDist = Join-Path $OutputDir "windows-x86_64"
        New-Item -ItemType Directory -Path $WinDist -Force | Out-Null
        Copy-Item $WinBinary (Join-Path $WinDist "p2p-clipboard.exe")

        $size = [math]::Round((Get-Item $WinBinary).Length / 1MB, 2)
        Write-Info "Windows 编译完成: $WinDist\p2p-clipboard.exe ($size MB)"
    }
    finally {
        Pop-Location
    }
}

function Build-Linux {
    Write-Info "===== 通过 WSL 编译 Linux x86_64 版本 ====="

    if (-not (Test-Command "wsl")) {
        Write-Err "未找到 WSL，请先安装: wsl --install"
        exit 1
    }

    # 检查 WSL 是否可用
    $wslStatus = wsl --status 2>&1
    if ($LASTEXITCODE -ne 0) {
        Write-Err "WSL 不可用，请检查 WSL 安装状态"
        exit 1
    }

    # 将 Windows 路径转为 WSL 路径
    $WslProjectRoot = wsl wslpath -u ($ProjectRoot -replace '\\', '/')
    $WslProjectRoot = $WslProjectRoot.Trim()

    Write-Info "WSL 项目路径: $WslProjectRoot"

    # 在 WSL 中执行编译（一次性完成依赖检查 + 编译）
    $BuildScript = @"
set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

log_info() { echo -e "`${GREEN}[INFO]`${NC} `$*"; }
log_error() { echo -e "`${RED}[ERROR]`${NC} `$*"; }

# 检查并安装 Rust
if ! command -v cargo &>/dev/null; then
    if [ -f "`$HOME/.cargo/env" ]; then
        . "`$HOME/.cargo/env"
    fi
fi

if ! command -v cargo &>/dev/null; then
    log_info "WSL 中未找到 Rust，正在安装..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "`$HOME/.cargo/env"
fi

log_info "Rust 版本: `$(rustc --version)"

# 检查并安装编译依赖
install_build_deps() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
    fi
    local ID="`${ID:-unknown}"

    case "`$ID" in
        ubuntu|debian)
            local NEEDED=""
            dpkg -s build-essential &>/dev/null || NEEDED="`$NEEDED build-essential"
            dpkg -s pkg-config &>/dev/null || NEEDED="`$NEEDED pkg-config"
            dpkg -s libxcb1-dev &>/dev/null || NEEDED="`$NEEDED libxcb1-dev"
            dpkg -s libxcb-render0-dev &>/dev/null || NEEDED="`$NEEDED libxcb-render0-dev"
            dpkg -s libxcb-shape0-dev &>/dev/null || NEEDED="`$NEEDED libxcb-shape0-dev"
            dpkg -s libxcb-xfixes0-dev &>/dev/null || NEEDED="`$NEEDED libxcb-xfixes0-dev"

            if [ -n "`$NEEDED" ]; then
                log_info "安装编译依赖:`$NEEDED"
                sudo apt-get update -qq
                sudo apt-get install -y -qq `$NEEDED
            else
                log_info "编译依赖已就绪"
            fi
            ;;
        fedora|rocky|rhel|centos|almalinux)
            local PKG_MGR="dnf"
            command -v dnf &>/dev/null || PKG_MGR="yum"
            sudo `$PKG_MGR install -y gcc gcc-c++ make pkgconfig libxcb-devel xcb-util-renderutil-devel
            ;;
        *)
            log_error "不支持的 WSL 发行版: `$ID，请手动安装 build-essential pkg-config libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev"
            exit 1
            ;;
    esac
}

install_build_deps

cd "$WslProjectRoot"
log_info "开始编译 Linux 版本..."
cargo build --release

if [ ! -f "target/release/p2p-clipboard" ]; then
    log_error "编译产物不存在"
    exit 1
fi

log_info "Linux 编译完成"
"@

    wsl bash -c $BuildScript
    if ($LASTEXITCODE -ne 0) {
        Write-Err "Linux 编译失败"
        exit 1
    }

    # 从 WSL 复制产物到 Windows
    $LinuxBinaryWsl = "$WslProjectRoot/target/release/p2p-clipboard"
    $LinuxDist = Join-Path $OutputDir "linux-x86_64"
    New-Item -ItemType Directory -Path $LinuxDist -Force | Out-Null

    $LinuxDistWsl = wsl wslpath -u ($LinuxDist -replace '\\', '/')
    $LinuxDistWsl = $LinuxDistWsl.Trim()
    wsl cp "$LinuxBinaryWsl" "$LinuxDistWsl/p2p-clipboard"

    if ($LASTEXITCODE -ne 0) {
        Write-Err "复制 Linux 产物失败"
        exit 1
    }

    $LinuxBinaryPath = Join-Path $LinuxDist "p2p-clipboard"
    $size = [math]::Round((Get-Item $LinuxBinaryPath).Length / 1MB, 2)
    Write-Info "Linux 编译完成: $LinuxDist\p2p-clipboard ($size MB)"
}

# 主逻辑
if (-not $Windows -and -not $Linux -and -not $All) {
    Write-Host "p2p-clipboard 构建脚本"
    Write-Host ""
    Write-Host "用法: .\scripts\build.ps1 [-Windows] [-Linux] [-All]"
    Write-Host ""
    Write-Host "选项:"
    Write-Host "  -Windows    仅编译 Windows 版本"
    Write-Host "  -Linux      仅编译 Linux 版本 (通过 WSL)"
    Write-Host "  -All        编译所有平台"
    exit 0
}

$startTime = Get-Date

Write-Info "p2p-clipboard v$Version 构建开始"
Write-Info "项目路径: $ProjectRoot"

New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null

if ($All -or $Windows) {
    Build-Windows
}

if ($All -or $Linux) {
    Build-Linux
}

$elapsed = [math]::Round(((Get-Date) - $startTime).TotalSeconds, 1)
Write-Host ""
Write-Info "===== 构建完成 (耗时 ${elapsed}s) ====="
Write-Info "产物目录: $OutputDir"

if (Test-Path (Join-Path $OutputDir "windows-x86_64")) {
    Write-Info "  Windows: $OutputDir\windows-x86_64\p2p-clipboard.exe"
}
if (Test-Path (Join-Path $OutputDir "linux-x86_64")) {
    Write-Info "  Linux:   $OutputDir\linux-x86_64\p2p-clipboard"
}
