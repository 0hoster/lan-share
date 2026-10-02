# shellcheck shell=bash
# 交叉编译共享环境：解析用户态工具链位置并注入 PATH。
#
# 所有工具都装在 CROSS_ROOT 下（默认 ~/.local/share/lan-share-cross），
# 不写系统目录、不需要 root，也不污染 cargo 的默认缓存。
#
#   CROSS_ROOT=/path/to/toolchain ./scripts/build-cross.sh

if [ -z "${CROSS_ROOT:-}" ]; then
  if [ -n "${XDG_DATA_HOME:-}" ]; then
    CROSS_ROOT="$XDG_DATA_HOME/lan-share-cross"
  elif [ -n "${HOME:-}" ]; then
    CROSS_ROOT="$HOME/.local/share/lan-share-cross"
  else
    CROSS_ROOT="/tmp/lan-share-cross"
  fi
fi

CROSS_RUSTUP_HOME="$CROSS_ROOT/rustup"
CROSS_CARGO_HOME="$CROSS_ROOT/cargo"
CROSS_ZIG_DIR="$CROSS_ROOT/zig"

export CROSS_ROOT CROSS_RUSTUP_HOME CROSS_CARGO_HOME CROSS_ZIG_DIR

# 需要交叉编译的目标平台
DEFAULT_TARGETS=(
  "x86_64-unknown-linux-gnu"
  "x86_64-unknown-linux-musl"
  "aarch64-unknown-linux-musl"
  "x86_64-pc-windows-gnu"
  "x86_64-apple-darwin"
  "aarch64-apple-darwin"
)

# 注入用户态 rustup / cargo / zig，覆盖系统 rust。
cross_activate() {
  # 只有存在用户态 rustup 时才覆盖环境；CI 上直接复用自带工具链。
  if [ -x "$CROSS_CARGO_HOME/bin/cargo" ]; then
    export RUSTUP_HOME="$CROSS_RUSTUP_HOME"
    export CARGO_HOME="$CROSS_CARGO_HOME"
    export PATH="$CROSS_CARGO_HOME/bin:$PATH"
  fi

  local zig_bin
  zig_bin="$(find "$CROSS_ZIG_DIR" -maxdepth 2 -type f -name zig 2>/dev/null | head -n 1 || true)"
  if [ -n "$zig_bin" ]; then
    export PATH="$(dirname "$zig_bin"):$PATH"
    export ZIG_BIN="$zig_bin"
  fi

  cross_pick_bins
}

# 探测本机 zig 目标名（用于从下载索引里挑对应包）
cross_host_tag() {
  local os arch
  case "$(uname -s)" in
    Linux) os="linux" ;;
    Darwin) os="macos" ;;
    *) os="unknown" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) arch="$(uname -m)" ;;
  esac
  echo "${arch}-${os}"
}

# 解析实际要使用的 cargo / rustup / cargo-zigbuild 可执行文件。
# 优先用户态工具链；若不存在（例如 CI 自带 rustup），回退到环境中的版本。
cross_pick_bins() {
  if [ -x "$CROSS_CARGO_HOME/bin/cargo" ]; then
    CROSS_CARGO_BIN="$CROSS_CARGO_HOME/bin/cargo"
    CROSS_RUSTUP_BIN="$CROSS_CARGO_HOME/bin/rustup"
  else
    CROSS_CARGO_BIN="$(command -v cargo || true)"
    CROSS_RUSTUP_BIN="$(command -v rustup || true)"
  fi
  CROSS_ZIGBUILD_BIN="$(command -v cargo-zigbuild || true)"
  export CROSS_CARGO_BIN CROSS_RUSTUP_BIN CROSS_ZIGBUILD_BIN
}
