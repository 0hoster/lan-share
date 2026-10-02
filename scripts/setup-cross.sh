#!/usr/bin/env bash
# 安装用户态交叉编译工具链（不需要 root，也不改动系统 rust）：
#   1. rustup + stable 工具链       → $CROSS_ROOT/rustup, $CROSS_ROOT/cargo
#   2. 各目标平台的标准库            → rustup target add
#   3. zig（跨平台 C 链接器/CRT）    → $CROSS_ROOT/zig
#   4. cargo-zigbuild（cargo 子命令）→ $CROSS_ROOT/cargo/bin
#
# 用法:
#   ./scripts/setup-cross.sh              # 安装全部默认目标
#   ZIG_VERSION=0.15.1 ./scripts/setup-cross.sh
#   RUSTUP_DIST_SERVER=https://mirrors.aliyun.com/rustup ./scripts/setup-cross.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./cross-env.sh
. "$here/cross-env.sh"

log()  { printf '\033[36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[!]\033[0m %s\n' "$*"; }
die()  { printf '\033[31m[x]\033[0m %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null || die "需要 curl"
command -v tar  >/dev/null || die "需要 tar"

# 国内网络可换成镜像：RUSTUP_DIST_SERVER=https://mirrors.aliyun.com/rustup
export RUSTUP_DIST_SERVER="${RUSTUP_DIST_SERVER:-https://static.rust-lang.org}"
export RUSTUP_UPDATE_ROOT="${RUSTUP_UPDATE_ROOT:-$RUSTUP_DIST_SERVER/rustup}"

cross_activate

host_tag="$(cross_host_tag)"
case "$host_tag" in
  x86_64-linux|aarch64-linux)   rustup_triple="${host_tag%-*}-unknown-linux-gnu" ;;
  x86_64-macos|aarch64-macos)   rustup_triple="${host_tag%-*}-apple-darwin" ;;
  *) die "不支持的本机平台: $host_tag" ;;
esac

mkdir -p "$CROSS_ROOT" "$CROSS_CARGO_HOME/bin" "$CROSS_ZIG_DIR"
log "工具链目录: $CROSS_ROOT"
log "本机平台: $host_tag (rustup triple: $rustup_triple)"

# ---------------------------------------------------------------- 1. rustup
# CI 上已有 rustup，可用 SKIP_RUSTUP=1 复用，避免重复下载整套工具链。
if [ "${SKIP_RUSTUP:-0}" = "1" ] && command -v rustup >/dev/null 2>&1; then
  log "SKIP_RUSTUP=1，复用已有 rustup"
elif [ -x "$CROSS_CARGO_HOME/bin/rustup" ] && [ -x "$CROSS_CARGO_HOME/bin/cargo" ]; then
  log "用户态 rustup 已安装，跳过"
else
  log "下载并安装 rustup（用户态，不改动系统 ~/.cargo）"
  tmp_init="$(mktemp -d)"
  trap 'rm -rf "$tmp_init"' EXIT
  curl -fsSL --retry 3 \
    "$RUSTUP_DIST_SERVER/rustup/dist/$rustup_triple/rustup-init" \
    -o "$tmp_init/rustup-init"
  chmod +x "$tmp_init/rustup-init"
  "$tmp_init/rustup-init" -y --no-modify-path --profile minimal \
    --default-toolchain stable --default-host "$rustup_triple"
  rm -rf "$tmp_init"
  trap - EXIT
fi

cross_activate
log "cargo: ${CROSS_CARGO_BIN:-cargo}  rustc: $(rustc --version)"

# ---------------------------------------------------------------- 2. 目标标准库
targets=("${@:-}")
if [ "${#targets[@]}" -eq 0 ] || [ -z "${targets[0]}" ]; then
  targets=("${DEFAULT_TARGETS[@]}")
fi

log "安装目标标准库: ${targets[*]}"
"$CROSS_RUSTUP_BIN" target add "${targets[@]}"

# ---------------------------------------------------------------- 3. zig
zig_index_url="${ZIG_INDEX_URL:-https://ziglang.org/download/index.json}"

resolve_zig_version() {
  local index="$1"
  if command -v python3 >/dev/null; then
    printf '%s' "$index" | python3 -c '
import json,sys
data=json.load(sys.stdin)
keys=[k for k in data if k!="master"]
keys.sort(key=lambda v:[int(x) for x in v.split(".") if x.isdigit()])
print(keys[-1])
'
  elif command -v jq >/dev/null; then
    printf '%s' "$index" | jq -r '[keys[] | select(. != "master")] | sort_by(split(".") | map(tonumber? // 0)) | last'
  else
    die "解析 zig 版本需要 python3 或 jq，或直接设置 ZIG_VERSION=x.y.z"
  fi
}

resolve_zig_url() {
  local index="$1" version="$2"
  if command -v python3 >/dev/null; then
    printf '%s' "$index" | python3 -c '
import json,sys
data=json.load(sys.stdin)
print(data[sys.argv[1]][sys.argv[2]]["tarball"])
' "$version" "$host_tag"
  else
    printf '%s' "$index" | jq -r --arg v "$version" --arg h "$host_tag" '.[$v][$h].tarball'
  fi
}

if [ -n "${ZIG_BIN:-}" ] && [ -x "${ZIG_BIN:-}" ]; then
  log "zig 已安装，跳过: $("$ZIG_BIN" version)"
else
  log "获取 zig 版本索引: $zig_index_url"
  zig_index="$(curl -fsSL --retry 3 "$zig_index_url")" || die "无法获取 zig 版本索引"
  zig_version="${ZIG_VERSION:-$(resolve_zig_version "$zig_index")}"
  [ -n "$zig_version" ] || die "解析 zig 版本失败"

  zig_url="$(resolve_zig_url "$zig_index" "$zig_version")" || die "zig $zig_version 没有 $host_tag 版本"

  log "下载 zig $zig_version ($host_tag)"
  curl -fsSL --retry 3 "$zig_url" -o "$CROSS_ROOT/zig.tar.xz"
  rm -rf "$CROSS_ZIG_DIR"
  mkdir -p "$CROSS_ZIG_DIR"
  tar -xJf "$CROSS_ROOT/zig.tar.xz" -C "$CROSS_ZIG_DIR" --strip-components=1
  rm -f "$CROSS_ROOT/zig.tar.xz"
  cross_activate
  log "zig: $("$ZIG_BIN" version)"
fi

# ---------------------------------------------------------------- 4. cargo-zigbuild
if [ -n "${CROSS_ZIGBUILD_BIN:-}" ]; then
  log "cargo-zigbuild 已安装，跳过"
else
  log "安装 cargo-zigbuild（首次编译约 1~2 分钟）"
  if [ -n "${CARGO_ZIGBUILD_VERSION:-}" ]; then
    "$CROSS_CARGO_BIN" install cargo-zigbuild --locked \
      --version "$CARGO_ZIGBUILD_VERSION"
  else
    "$CROSS_CARGO_BIN" install cargo-zigbuild --locked
  fi
  cross_activate
fi

echo
log "安装完成 ✔"
cat <<EOF

  工具链根目录  $CROSS_ROOT
  rustc        $(rustc --version)
  zig          $("${ZIG_BIN:-zig}" version)
  已装目标      $("$CROSS_RUSTUP_BIN" target list --installed | tr '\n' ' ')

  下一步:  ./scripts/build-cross.sh          # 构建全部目标
           ./scripts/build-cross.sh x86_64-pc-windows-gnu

EOF
