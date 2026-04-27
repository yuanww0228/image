#!/usr/bin/env bash
set -euo pipefail

OUT_FILE="${OUT_FILE:-$PWD/cf-preferred-result.csv}"
CDN_DOMAIN="${CDN_DOMAIN:-}"
TOP_N="${TOP_N:-10}"
LATENCY_MAX="${LATENCY_MAX:-300}"
LOSS_MAX="${LOSS_MAX:-0.2}"
DOWNLOAD_TEST_N="${DOWNLOAD_TEST_N:-10}"
DOWNLOAD_TIME="${DOWNLOAD_TIME:-8}"
PORT="${PORT:-443}"

die() {
  echo "[错误] $*" >&2
  exit 1
}

if command -v pkg >/dev/null 2>&1; then
  echo "[0/4] 检查 Termux 依赖..."
  pkg install -y curl tar coreutils >/dev/null
fi

command -v curl >/dev/null 2>&1 || die "缺少 curl，请先安装 Termux 并执行: pkg install curl tar"
command -v tar >/dev/null 2>&1 || die "缺少 tar，请先安装: pkg install tar"

case "$(uname -m)" in
  aarch64|arm64)
    PKG="cfst_linux_arm64.tar.gz"
    ;;
  armv7l|armv8l)
    PKG="cfst_linux_armv7.tar.gz"
    ;;
  armv6l)
    PKG="cfst_linux_armv6.tar.gz"
    ;;
  *)
    die "不支持或未识别的安卓 CPU 架构: $(uname -m)"
    ;;
esac

mkdir -p "$HOME/cfst"
cd "$HOME/cfst"

if [ ! -x ./cfst ]; then
  echo "[1/4] 下载 CloudflareSpeedTest: ${PKG}"
  curl -fL --connect-timeout 10 --retry 3 \
    -o "$PKG" \
    "https://github.com/XIU2/CloudflareSpeedTest/releases/latest/download/${PKG}"
  tar -zxf "$PKG"
  chmod +x ./cfst
fi

echo "[2/4] 下载 Cloudflare 官方 IPv4 段..."
curl -fsSL https://www.cloudflare.com/ips-v4 -o ip.txt

ARGS=(
  -f ip.txt
  -tp "$PORT"
  -tl "$LATENCY_MAX"
  -tlr "$LOSS_MAX"
  -dn "$DOWNLOAD_TEST_N"
  -dt "$DOWNLOAD_TIME"
  -p "$TOP_N"
  -o "$OUT_FILE"
)

if [ -n "$CDN_DOMAIN" ]; then
  ARGS+=(-url "https://${CDN_DOMAIN}/cdn-cgi/trace" -httping)
fi

echo "[3/4] 开始测速。测速前请关闭 Hiddify/Clash/VPN，否则结果会不准。"
./cfst "${ARGS[@]}"

echo
echo "[4/4] 推荐填入安装脚本的 CF 优选 IP/域名："
awk -F, 'NR == 2 {gsub(/^[ \t]+|[ \t]+$/, "", $1); print $1}' "$OUT_FILE"

echo
echo "完整结果: $OUT_FILE"
echo "如果结果为空，可放宽条件再跑："
echo "LATENCY_MAX=500 LOSS_MAX=0.5 bash find_cf_preferred_ip_termux.sh"
