#!/usr/bin/env bash
#
# 编译检查 `--features needle-lib` 那条路径 —— 不需要 FFmpeg 开发库。
#
# ── 为什么需要这个脚本 ────────────────────────────────────────────────
#
# `detect/needle_lib.rs` 和 `worker.rs` 里对应的分支都被
# `#[cfg(feature = "needle-lib")]` 门控。默认构建不开这个 feature，所以
# **那几百行代码从来不参与类型检查** —— 参数个数错了、类型错了、
# 读了不该读的私有字段，编译器全都不会吭声，一直等到有人在装齐依赖的
# 机器上第一次开这个 feature 才炸。
#
# 而装齐依赖本身很贵：真 needle-rs 要 ffmpeg-sys-next（pkg-config +
# FFmpeg 开发库）和 chromaprint-sys-next（cmake 从源码编译），Windows 上
# 还要 cargo-vcpkg 编整个 FFmpeg，以小时计。
#
# ── 它怎么绕过这件事 ──────────────────────────────────────────────────
#
# 把项目源码复制一份到临时目录，把 core 里 needle-rs 的 git 依赖换成
# tools/needle-api-stub —— 一份签名逐字抄自上游的假 crate。然后正常
# cargo check。真正的调用点会照常被类型检查，只是被调的那个库换成了空壳。
#
# ── 它验证什么、不验证什么（重要，别过度信任） ────────────────────────
#
#   验证   —— 我们的调用方式与上游公开 API 是否匹配：参数个数、类型、
#             泛型约束、字段可见性、feature 名。
#   不验证 —— 上游的实际运行行为。这个仍然得在装齐 FFmpeg 开发库的机器上
#             开真 feature 跑一遍。
#
# 换句话说：它能挡住「编译不过」，挡不住「编过了但行为不对」。
#
# ── 用法 ──────────────────────────────────────────────────────────────
#
#   bash tools/check-needle-lib.sh
#
# 环境变量：
#   NEEDLE_LIB_CHECK_DIR    工作目录。**必需在项目目录之外**。
#                           指到固定路径后依赖缓存会留下来，重跑从 90 秒降到几秒。
#   NEEDLE_LIB_CHECK_KEEP   设为 1 则无论成败都保留临时目录

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# 工作目录**必须放在项目外面**。
#
# 踩过的坑：一开始把它放在 `<项目>/target/needle-lib-check`，看着很自然
# （target/ 本来就被 gitignore 了），结果 cargo 直接拒绝：
#   error: multiple workspace roots found in the same workspace
# 放到系统临时目录下、或者项目旁边的任意目录就没事。
if [ -n "${NEEDLE_LIB_CHECK_DIR:-}" ]; then
  work="$NEEDLE_LIB_CHECK_DIR"
  reuse_cache=1
else
  work="$(mktemp -d "${TMPDIR:-/tmp}/ior-needle-lib-check.XXXXXX")"
  reuse_cache=0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "找不到 cargo。先装 Rust：https://rustup.rs" >&2
  exit 1
fi

echo "项目根目录：$root"
echo "工作目录  ：$work"

# 每轮只重建源码副本与 stub 副本，**保留 target/**。
#
# 保留 target 是有意的：依赖树（egui/wgpu 那一大堆）编译一次要一分半，
# 而这一分半跟「我们自己的代码写得对不对」毫无关系。把
# NEEDLE_LIB_CHECK_DIR 指到一个固定目录，第二次往后几秒就能出结果，
# 排查时来回试才不至于被构建时间劝退。
# 源码副本则是每次都要重建的 —— 万一上一轮的残留混进去，结论就不可信了。
rm -rf "$work/src" "$work/needle-stub"
mkdir -p "$work/src"

# 只复制编译需要的东西。target/ 是上一次的构建缓存（可能几百 MB），
# 复制它除了浪费时间没有任何意义。
#
# **必须把 needle-api-stub 排除掉，另行放到项目树的旁边**（见下面 cp）。
# 踩过的坑：把它留在复制品的 tools/ 里，cargo 会拒绝：
#   error: multiple workspace roots found in the same workspace
# 因为 stub 自己带一个 [workspace]，而它在物理上又落在复制品这个 workspace
# 的目录树内，cargo 认为两个 workspace 根互相冲突。
# 放到目录树外面，它就是一个普通的、独立的 path 依赖。
tar -C "$root" \
  --exclude=./target \
  --exclude=./.git \
  --exclude=./docs \
  --exclude=./tools/needle-api-stub \
  -cf - ./Cargo.toml ./Cargo.lock ./crates ./tools \
  | tar -C "$work/src" -xf -

cp -r "$root/tools/needle-api-stub" "$work/needle-stub"

manifest="$work/src/crates/core/Cargo.toml"

# 把 git 依赖换成 stub 的 path 依赖。
# 匹配的是行首的 `needle = { git = ...`，只动这一行，注释和其它依赖不碰。
if ! grep -q '^needle = { git = ' "$manifest"; then
  echo "在 $manifest 里找不到 needle 的 git 依赖行 ——" >&2
  echo "是不是依赖声明被改过了？这个脚本需要跟着一起改。" >&2
  exit 1
fi

# 用临时文件而不是 sed -i：GNU sed 与 BSD sed 的 -i 参数不兼容，
# 而这个脚本在 Windows(Git Bash) / Linux / macOS 上都要能用。
awk '
  /^needle = \{ git = / {
    print "needle = { path = \"../../../needle-stub\", package = \"needle-rs\", optional = true }"
    next
  }
  { print }
' "$manifest" > "$manifest.tmp"
mv "$manifest.tmp" "$manifest"

echo
echo "已把 needle-rs 换成 tools/needle-api-stub，开始检查……"
echo

# CARGO_TARGET_DIR 指到工作目录里：不污染项目自己的 target/，
# 也让这个脚本的产物可以整目录删掉。
if cargo clippy --version >/dev/null 2>&1; then
  # clippy 覆盖 check 能查的东西，顺带把 lint 也过一遍
  runner=(cargo clippy --all-targets)
else
  echo "（没装 clippy，退回 cargo check。想查得更细就 rustup component add clippy）"
  runner=(cargo check --all-targets)
fi

set +e
(
  cd "$work/src" &&
    CARGO_TARGET_DIR="$work/target" \
      "${runner[@]}" -p intro-outro-gui --features needle-lib
)
status=$?
set -e

echo
if [ "$status" -eq 0 ]; then
  echo "=== needle-lib 路径编译检查通过 ==="
  echo "注意这只说明签名对得上，不说明上游运行时行为正确。"
else
  echo "=== needle-lib 路径编译检查失败（退出码 $status）===" >&2
  echo "报错里的行号是**复制品**的行号；因为文件是原样复制的，" >&2
  echo "所以在 crates/core/src/detect/needle_lib.rs 里行号一致。" >&2
fi

# 收尾：
# - 用户指定了 NEEDLE_LIB_CHECK_DIR → 保留 target/ 里的依赖缓存（这是它存在的
#   意义），只清掉源码副本；下次重跑几秒出结果。
# - 失败 → 保留现场，出错了多半要进去翻一眼。
# - 其余情况（自动建的临时目录 + 通过）→ 整个删掉，不留垃圾。
if [ "$reuse_cache" = "1" ]; then
  rm -rf "$work/src" "$work/needle-stub"
  echo "依赖缓存留在 $work/target，下次重跑会复用（想彻底清掉就删掉整个 $work）"
elif [ "${NEEDLE_LIB_CHECK_KEEP:-0}" = "1" ] || [ "$status" -ne 0 ]; then
  echo "工作目录已保留：$work"
else
  rm -rf "$work"
fi

exit "$status"
