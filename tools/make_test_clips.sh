#!/usr/bin/env bash
# 造一批「假剧集」，用于端到端实测（配合 crates/core/tests/e2e.rs）。
#
# 每集的音频结构：
#
#     [ 公共片头 25s ] [ 各集独有正片 30+5i 秒 ] [ 公共片尾 12s ]
#
# 片头片尾在每一集里是**逐字节相同**的音频，所以 chromaprint 指纹比对应该能把它们
# 找出来；正片逐集换一组参数，保证各集之间没有别的公共片段，避免误检。
#
# # 为什么用「调频音」而不是干净的正弦音
#
# 这条是实测踩出来的：一开始用固定频率的正弦（440Hz + 660Hz）当素材，结果
# needle 检出来的片头位置**毫无规律** —— 四集的片头起点分别是 3s/3s/3s/17s，
# 而它们其实是同一段音频。原因是 chromaprint 处理**平稳信号**（频率不变的纯音）
# 时，每个时间窗算出来的指纹都是同一个值，哈希序列退化成一条常量。
# 于是「最长公共子序列」可以在这条常量序列的任意位置对齐，匹配结果自然乱跳。
#
# 改成两个**调频**音（瞬时频率随时间连续变化）之后，信号不再平稳，哈希序列有了
# 明确的时间结构，四集的片头起点立刻收敛到同一个值。真实影视音频本来就是非平稳的，
# 所以这个素材反而更贴近实际。
#
# # 两个刻意的设计
#
# 1. **片尾只给 12 秒**，低于 needle 的默认阈值（20 秒）。这样跑测试时必须显式
#    传 `--min-ending-duration` 才检得到 —— 顺带验证了参数确实在起作用，
#    而不是碰巧撞上默认值。
# 2. **关键帧间隔压到 1 秒**（`-g 25`，25fps）。否则 x264 默认的 GOP 会长达
#    数秒，流复制切出来的起点会吸附回去好几秒，断言就只好放宽到没意义的程度。
#    真实的蓝光压制片源关键帧通常也就 1～2 秒一个，这个设置是贴近现实的。
#
# 用法：
#     bash tools/make_test_clips.sh <ffmpeg 可执行文件> <输出目录> [集数]

set -euo pipefail

FFMPEG="${1:?用法: make_test_clips.sh <ffmpeg> <输出目录> [集数]}"
OUTDIR="${2:?用法: make_test_clips.sh <ffmpeg> <输出目录> [集数]}"
EPISODES="${3:-4}"

INTRO_SECS=25
OUTRO_SECS=12
# 各集正片长度 = MAIN_BASE + i * MAIN_STEP，逐集拉开，便于验证「结果与文件一一对应」
MAIN_BASE=30
MAIN_STEP=5

mkdir -p "$OUTDIR"
cd "$OUTDIR"

# 造一段「调频双音」音频。
#
# 两个分量各自带正弦调制的瞬时频率，所以频谱随时间连续移动，绝不重复自己。
# 同一组参数永远得到同一段波形（纯确定性函数），不同组之间不会互相匹配。
#
#   $1 时长(秒) $2 分量A基频 $3 A调制深度 $4 A调制速率
#   $5 分量B基频 $6 B调制深度 $7 B调制速率 $8 输出文件
make_tone() {
  "$FFMPEG" -y -loglevel error \
    -f lavfi -i "aevalsrc='0.5*sin(2*PI*($2+$3*sin($4*t))*t)+0.4*sin(2*PI*($5+$6*sin($7*t+1.3))*t)':d=$1:s=44100" \
    -c:a pcm_s16le "$8"
}

echo "== 公共片头（${INTRO_SECS}s，调频双音）=="
make_tone "$INTRO_SECS" 260 90 0.55 390 140 0.83 intro.wav

echo "== 公共片尾（${OUTRO_SECS}s，调频双音）=="
make_tone "$OUTRO_SECS" 150 60 0.71 210 100 0.47 outro.wav

for i in $(seq 0 $((EPISODES - 1))); do
  n=$((i + 1))
  main_dur=$((MAIN_BASE + i * MAIN_STEP))
  total=$((INTRO_SECS + main_dur + OUTRO_SECS))

  echo "== 第 ${n} 集：片头 ${INTRO_SECS}s + 正片 ${main_dur}s + 片尾 ${OUTRO_SECS}s = ${total}s =="

  # 正片逐集换一组参数（基频随集数上移，调制速率也变），保证各集互不相似
  make_tone "$main_dur" $((280 + n * 55)) 110 "$((19 + n * 7))e-2" \
                         $((410 + n * 70)) 150 "$((51 + n * 9))e-2" "main_${n}.wav"

  "$FFMPEG" -y -loglevel error \
    -i intro.wav -i "main_${n}.wav" -i outro.wav \
    -filter_complex "[0][1][2]concat=n=3:v=0:a=1" \
    -c:a pcm_s16le "ep${n}_audio.wav"

  # 画面用随集数变化的纯色，便于肉眼分辨切点对不对
  color=$(printf "0x%02x4f8a" $((30 + i * 40)))
  "$FFMPEG" -y -loglevel error \
    -f lavfi -i "color=c=${color}:s=320x240:r=25:d=${total}" \
    -i "ep${n}_audio.wav" \
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
    -g 25 -keyint_min 25 -sc_threshold 0 \
    -c:a aac -b:a 128k \
    -shortest "Show.S01E0${n}.mkv"

  rm -f "main_${n}.wav" "ep${n}_audio.wav"
done

rm -f intro.wav outro.wav

echo
echo "== 生成完毕 =="
for f in *.mkv; do
  dur=$("$FFMPEG" -hide_banner -i "$f" 2>&1 | grep -o 'Duration: [0-9:.]*' | head -1 || true)
  size=$(stat -c%s "$f" 2>/dev/null || echo "?")
  echo "  $f  ${dur}  ${size} 字节"
done
echo
echo "预期：片头 ≈ 00:00:00 - 00:00:${INTRO_SECS}，片尾 ≈ 最后 ${OUTRO_SECS} 秒"
echo

# Git Bash 下 pwd 给的是 /c/... 形式，Rust 在 Windows 上会把开头的 /c 当成
# 「当前盘根目录下的 c 目录」而找不到文件。有 cygpath 就换成 Windows 路径。
wpath() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else echo "$1"; fi
}

echo "接下来："
echo "  export IOR_E2E_CLIPS=\"$(wpath "$(pwd)")\""
echo "  export IOR_E2E_DIR=\"$(wpath "$(pwd)/../e2e-out")\""
echo "  export IOR_FFMPEG_DIR=\"$(wpath "$(dirname "$FFMPEG")")\""
echo "  export IOR_NEEDLE_DIR=\"<needle.exe 所在目录>\""
echo "  cargo test --test e2e -- --ignored --nocapture"
