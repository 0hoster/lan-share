#!/usr/bin/env bash
# 一键交叉编译：从当前机器产出 Linux / Windows / macOS 多平台二进制到 dist/。
#
# 用法:
#   ./scripts/build-cross.sh                     # 构建全部默认目标（release）
#   ./scripts/build-cross.sh aarch64-apple-darwin
#   PROFILE=dev ./scripts/build-cross.sh         # 调试构建（跳过打包）
#   ./scripts/build-cross.sh --list              # 查看目标列表
#
# 依赖: 先执行 ./scripts/setup-cross.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
# shellcheck source=./cross-env.sh
. "$here/cross-env.sh"

log()  { printf '\033[36m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[31m[x]\033[0m %s\n' "$*" >&2; exit 1; }

PROFILE="${PROFILE:-release}"
targets=()
for arg in "$@"; do
  case "$arg" in
    --list) printf '%s\n' "${DEFAULT_TARGETS[@]}"; exit 0 ;;
    --debug) PROFILE="dev" ;;
    -*) die "未知参数: $arg" ;;
    *) targets+=("$arg") ;;
  esac
done
[ "${#targets[@]}" -gt 0 ] || targets=("${DEFAULT_TARGETS[@]}")

cross_activate

cargo_bin="${CROSS_CARGO_BIN:-}"
[ -n "$cargo_bin" ] || die "未找到 cargo，请先运行 ./scripts/setup-cross.sh"

if [ -z "${CROSS_ZIGBUILD_BIN:-}" ]; then
  die "缺少 cargo-zigbuild，请先运行 ./scripts/setup-cross.sh"
fi
[ -n "${ZIG_BIN:-}" ] || die "未找到 zig，请先运行 ./scripts/setup-cross.sh"

version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)"
[ -n "$version" ] || die "无法从 Cargo.toml 解析版本号"

log "工具链: $("$cargo_bin" --version) / zig $("$ZIG_BIN" version)"
log "版本: $version   profile: $PROFILE"
log "目标: ${targets[*]}"

dist="${DIST:-$root/dist}"
mkdir -p "$dist"
built=()
failed=()

for target in "${targets[@]}"; do
  log "构建 $target …"
  if (cd "$root" && "$cargo_bin" zigbuild --profile "$PROFILE" --target "$target"); then
    ext=""
    case "$target" in *windows*) ext=".exe" ;; esac
    src="$root/target/$target/$PROFILE/lan-share$ext"
    if [ ! -f "$src" ] && [ "$PROFILE" = "dev" ]; then
      src="$root/target/$target/debug/lan-share$ext"
    fi
    [ -f "$src" ] || { failed+=("$target"); continue; }

    out="$dist/lan-share-$version-$target$ext"
    cp "$src" "$out"
    size="$(du -h "$out" | cut -f1)"
    printf '    \033[32m✔\033[0m %s (%s)\n' "$(basename "$out")" "$size"
    built+=("$out")

    # 附带压缩包：Windows 用 zip，其余用 tar.gz
    if [ "$PROFILE" = "release" ]; then
      if [ -n "$ext" ] && command -v zip >/dev/null; then
        (cd "$dist" && zip -q -j "lan-share-$version-$target.zip" "$(basename "$out")")
      elif [ -z "$ext" ]; then
        tar -czf "$dist/lan-share-$version-$target.tar.gz" -C "$dist" "$(basename "$out")"
      fi
    fi
  else
    failed+=("$target")
  fi
done

# ---------------------------------------------------------------- 校验与汇总
if [ "${#built[@]}" -gt 0 ] && [ "$PROFILE" = "release" ]; then
  (cd "$dist" && sha256sum lan-share-* > SHA256SUMS.txt)
  log "已生成校验文件 dist/SHA256SUMS.txt"
fi

echo
log "构建结果"
for path in "${built[@]:-}"; do
  [ -n "$path" ] || continue
  printf '  %-52s %s\n' "$(basename "$path")" "$(file -b "$path" | cut -c1-70)"
done

if [ "${#failed[@]}" -gt 0 ]; then
  echo
  printf '\033[31m失败目标:\033[0m %s\n' "${failed[*]}" >&2
  exit 1
fi

echo
log "全部完成，产物位于 $dist"
