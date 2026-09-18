# 剧集片头片尾批量去除工具

把一整季电视剧的**片头**和**片尾**自动找出来、一次性切掉，输出到新目录。
源文件一个字节都不动。

纯 Rust + egui 写的桌面程序，Windows / macOS / Linux 都能跑。

```
导入一季的剧集  →  自动检测每集的公共片头片尾（靠音频指纹跨集比对）
                →  表格里逐个确认、可手改时间戳、可抽帧预览切点
                →  批量切割（流复制，秒级，无损）→ 输出到新目录
```

---

## 一、先装两个外部工具

本程序自己不带 ffmpeg，也不带 needle —— 都是调用系统上已有的。这样做是为了
让构建只需要几十秒（把 ffmpeg 静态链进来会把构建时间推到几小时）。

### 1. ffmpeg（必需）

负责读视频信息、切割、抽帧预览。**必须同时有 `ffmpeg` 和 `ffprobe`**。

| 平台 | 装法 |
|---|---|
| Windows | 到 <https://www.gyan.dev/ffmpeg/builds/> 下载 `ffmpeg-release-essentials.zip`，解压后把 `bin` 目录加进 PATH |
| macOS | `brew install ffmpeg` |
| Debian/Ubuntu | `sudo apt install ffmpeg` |

> **国内下载太慢的话**：gyan.dev 的包有 106 MB，实测国内直连只有 30 KB/s 左右
> （要一小时），而且连接不稳会断。用 npm 镜像拿同样的 gyan.dev 构建只要几秒 ——
> 实测 12 MB/s：
>
> ```bash
> base=https://registry.npmmirror.com/-/binary/ffmpeg-static/b6.1.1
> curl -L -o ffmpeg.exe  "$base/ffmpeg-win32-x64"
> curl -L -o ffprobe.exe "$base/ffprobe-win32-x64"
> ```
>
> 得到的是 ffmpeg / ffprobe **6.1.1 essentials build（配套同版本）**，含 libx264 与 aac。

验证：

```bash
ffmpeg -version
ffprobe -version
```

不想配 PATH 也行 —— 界面上有一栏「ffmpeg 目录」，指到 `bin` 目录（或者它的上一级）即可。

### 2. needle（想用自动检测就需要）

负责「跨集比对找片头片尾」。同样可以手动填时间戳来绕过它，但那就没意思了。

**Windows 最省事的做法**：到
<https://github.com/aksiksi/needle/releases> 下载 `needle-v0.1.5-windows-amd64.zip`
（约 7 MB，单文件，**自带静态 FFmpeg，不需要额外依赖**），解压出 `needle.exe`
放进 PATH，或者直接把所在目录填进界面的「needle 目录」一栏。

**其他平台**：

```bash
# 有 Docker（amd64）
docker run ghcr.io/aksiksi/needle:latest --help

# 或者自己编（要先装 FFmpeg 开发库，见「二、构建」里的说明）
cargo install needle-rs
```

验证：

```bash
needle --version     # 应输出 needle-rs 0.1.5
```

> **官方二进制和 GitHub main 分支的命令行参数不一样。** 实测
> `needle-v0.1.5-windows-amd64.zip` 里 `analyze` 只接受
> `--mode / --hash-period / --hash-duration / --threaded-decoding / --force`；
> 而 main 分支的 `analyze` 改成了 `--include-endings` 那一套。
>
> 本程序**不假设**你用的是哪个版本 —— 跑检测前会先读一次 `needle analyze --help`
> 和 `needle search --help`，按实际支持的参数拼命令，两个版本都能正常用。
> 日志区第一行会显示探测结果（`needle 参数探测：analyze支持 --include-endings…`）。
> 所以：**用官方 release 的二进制就行，不必自己编译。**

---

## 二、构建

```bash
git clone <本仓库>
cd intro-outro-remover
cargo build --release
```

产物在 `target/release/intro-outro-gui`（Windows 上是 `intro-outro-gui.exe`）。

默认这条路**没有任何原生依赖**，不需要 cmake、不需要 FFmpeg 开发库，
第一次构建大概一两分钟（要下载并编译 egui 那一堆依赖）。

### 换个检测后端（可选，构建很重）

默认的检测后端是「子进程调用 needle 可执行文件」。如果你想改成
**直接把 needle-rs 链接进来**：

```bash
cargo build --release --features needle-lib
```

它唯一的好处是不产生 `.needle.dat` 临时文件。但要付这些代价：

- **必须装 FFmpeg 开发库**。Linux 上 `apt install libavutil-dev libavformat-dev
  libswresample-dev libavcodec-dev libclang-dev cmake pkg-config`；Windows 上要装
  `cargo-vcpkg` 再跑 `cargo vcpkg build`，那会用 vcpkg **从源码编译整个 FFmpeg**，
  构建时间以小时计。
- **拿不到逐集进度，也没法中途取消**。needle-rs 的 `Analyzer::run` 和
  `Comparator::run_with_frame_hashes` 都是同步阻塞调用，没有回调也没有取消入口。
- **源目录里还是会短暂出现 `.needle.skip.json`**，因为 needle 的
  `SearchResult` 字段是私有的、没有任何 getter，结果只能靠它写文件再读回来。

结论：除非你特别在意那个临时文件，否则**用默认的就行**。

#### 不想装那一堆原生依赖，又想确认它没写坏

这个 feature 门控的代码（`detect/needle_lib.rs` 和 `worker.rs` 里对应的分支）
默认构建时**根本不参与编译**，所以它写错了编译器也不会告诉你。有个脚本能单独把它
检查一遍，**零原生依赖**：

```bash
bash tools/check-needle-lib.sh
```

首次约 90 秒（那是在编译依赖树，跟你的代码无关）。把工作目录固定下来之后，
重跑只要 20 秒左右：

```bash
NEEDLE_LIB_CHECK_DIR=/tmp/ior-check bash tools/check-needle-lib.sh
```

原理是把项目复制一份到临时目录，把 needle-rs 换成 `tools/needle-api-stub`
（一份签名逐字抄自上游的假 crate），然后正常编译。

说清它的边界：它只验证**我们的调用与上游公开 API 是否匹配**（参数个数、类型、
泛型约束、字段可见性、feature 名），**不验证上游的运行时行为**。也就是说它能挡住
「编译不过」，挡不住「编过了但行为不对」—— 后者仍然得在装齐 FFmpeg 开发库的机器上
开真 feature 跑一遍。

### 只想跑测试

```bash
cargo test
```

涵盖时间戳解析、保留区间计算、ffprobe JSON 解析、ffmpeg 进度行解析、
旁挂文件路径对齐等纯逻辑部分 —— 这些不依赖外部工具，任何机器上都能跑。

还有一条**真的端到端**用例（默认为 `#[ignore]`，因为它会真跑 ffmpeg 和 needle）：

```bash
bash tools/make_test_clips.sh          # 合成 4 集带公共片头片尾的素材
export IOR_E2E_CLIPS=$(pwd)/clips
export IOR_E2E_DIR=$(pwd)/out
export IOR_FFMPEG_DIR=/path/to/ffmpeg/bin
export IOR_NEEDLE_DIR=/path/to/needle
cargo test -p intro-outro-core --test e2e -- --ignored --nocapture
```

它会走完「扫描 → 探测 → 检测 → 出计划 → 切割」全流程，最后断言源文件
**字节数和修改时间都没变**、源目录**没留下任何旁挂文件**。本机实测约 11 秒。
（Windows 下用 Git Bash 跑的话路径会被自动翻译，不必手动 `cygpath`。）

---

## 三、用起来

1. **导入**：点「添加文件夹…」选一季的目录，或者直接把文件/文件夹拖进窗口。
   至少要有 **2 集** —— 跨集比对靠的是「这两集有一段一模一样的音频」，只有一集
   没法比。只有一集时可以手动填时间戳然后直接切。
2. **检测**：点「开始检测」。日志区会实时显示分析到第几集。
3. **确认切点**（重要，别跳过）：
   - 表格里每一行的片头、片尾时间戳都是**可编辑**的，格式随你
     （`00:01:30`、`1:30`、`90.5` 都能认）。
   - 点某行的「预览」，右侧会抽出**片头刚结束**那一帧。这就是切完之后
     正片的第一帧 —— 如果画面上还在唱片头曲，说明终点标早了；如果是黑屏，
     说明标晚了。
   - 改完切点可以再点一次「预览」复核。
4. **切割**：选好输出目录（默认跟源文件同目录），点「开始切割」。
   源文件不会被改动，所有输出写到输出目录，文件名加 `_trimmed` 后缀。

---

## 四、需要知道的几件事

### 1. 检测出来的边界会「往里缩」一点（这是 needle 的设计，不是 bug）

needle 报出的片头/片尾区间，比真实位置**两边各缩进大约一个哈希窗口**。默认配置下：

| | 真实位置 | needle 报的 | 结果 |
|---|---|---|---|
| 片头 | 0:00 – 0:25 | 0:03 – 0:24 | 开头约 3 秒会留下 |
| 片尾 | 0:55 – 1:07 | 0:57 – 1:02 | 末尾约 4 秒会留下 |

原因在它的边界表示方式：比对结果直接拿**哈希点自带的时间戳**当边界，而
chromaprint 的指纹点是在一整个窗口的音频处理完之后才产出的 —— 时间戳记的是窗口
**末端**。所以「匹配到的第一个点」的时间戳天然就是一个窗口宽（默认 3 秒），
起点不可能报得更早。

实测验证过这个结论：`--hash-duration` 设 3 起点就是 3.0 秒，设 6 起点就是 6.0 秒。
而且官方 v0.1.5 把 `hash_duration` 的**下限钉死在 3 秒**
（报错原文 `hash_duration must be greater than 3 seconds`），所以这条路走不通。

**想要更干净的切口**：在表格里把那一行的片头起点改成 `0`、片尾终点改成视频总长
（时间戳可以直接编辑），再点「预览」确认一下切口画面对不对。批量做也只需每行改两次。

> 顺带一提：这个「缩进」对 needle 原本的用途（播放器跳过片头）是**优点** ——
> 宁可少跳一秒，也不能跳进正片里。我们做的是剪掉文件，所以需要手动收紧。

### 2. 「快速模式」的起点会对齐到关键帧

默认的快速模式用 `-c copy` 做流复制 —— 不重新编码，画质无损，秒级完成。
代价是视频**无法从任意位置开始**，只能从关键帧开始。所以切出来的正片起点
可能比标定的时间早 1～5 秒（取决于片源的 GOP 长度）。

- 如果发现**正片开头被切掉了一点**：把该行的片头终点往后调 2～3 秒。
- 如果发现**接了半秒片头尾巴**：把片头终点往前调一点。
- 真要做到帧精确：把切割模式切到「精确（重新编码）」。它用 libx264 CRF 18
  重编码，帧精确，但一集 45 分钟 1080p 在普通 CPU 上要几分钟 —— 整季就是几小时。

也可以用检测参数里的「时间边距」统一把片头起点往后、片尾终点往前各收一点。

### 3. 剪掉片头片尾时，字幕和多音轨会一起保留

切割命令用的是 `-map 0`，也就是复制**所有**流：正片、多音轨、字幕、章节都带走。
输出文件的扩展名跟源文件一致（mkv 还是 mkv），就是为了这个 ——
转成 mp4 会让 PGS 字幕之类的流放不下。

万一 ffmpeg 报「某些流无法放进目标容器」，说明这个源文件的封装和它的流不完全兼容，
换成精确模式试试，或者先用 ffmpeg 转封装。

### 4. 检测结果不理想时，先调这两个参数

在「检测参数」折叠区里：

| 参数 | 什么时候调 |
|---|---|
| **片头最短时长** | 误报太多（明明没片头却检出 20 秒）→ 调到接近真实片头长度（常见 85～90 秒）；漏检 → 调小 |
| **哈希匹配阈值** | 检不出来 → 调大一点（比如 12～14）；误报 → 调小（比如 8） |

还有一个坑值得单独说：**如果你之前用「只检测片头」跑过一次，缓存里没有片尾数据，
再打开片尾检测时会一直搜不到片尾。** 勾上「强制重新分析」重算一遍就好。

---

## 五、目录结构

```
intro-outro-remover/
├── Cargo.toml                     # workspace 根
├── crates/
│   ├── core/                      # 核心引擎，不依赖任何 GUI 库
│   │   ├── src/
│   │   │   ├── lib.rs             # 模块说明 + 快速上手示例
│   │   │   ├── error.rs           # 统一错误类型
│   │   │   ├── model.rs           # 时间戳解析 / 保留区间计算（纯逻辑，有测试）
│   │   │   ├── probe.rs           # 定位并调用 ffmpeg / ffprobe
│   │   │   ├── exec.rs            # 子进程流式执行 + 可取消
│   │   │   ├── skipfile.rs        # 读写 needle 的结果旁挂文件
│   │   │   ├── detect.rs          # 检测接口：事件、参数、清理
│   │   │   ├── detect/cli.rs      # 后端 A：子进程调 needle（默认）
│   │   │   ├── detect/needle_lib.rs  # 后端 B：链接 needle-rs（可选）
│   │   │   ├── cut.rs             # ffmpeg 切割：两种模式、进度、拼接
│   │   │   └── pipeline.rs        # 编排：扫目录→探测→检测→出计划→批量切
│   │   └── tests/e2e.rs           # 真跑 ffmpeg + needle 的端到端用例
│   └── gui/                       # egui 桌面界面
│       └── src/
│           ├── main.rs            # 入口：日志、窗口
│           ├── app.rs             # eframe::App：布局与状态机
│           ├── worker.rs          # 后台线程与消息通道
│           └── fonts.rs           # 中文字体加载
├── tools/
│   ├── make_test_clips.sh         # 合成测试素材（4 集带公共片头片尾）
│   ├── check-needle-lib.sh        # 不开 needle-lib 也能检查它的编译
│   └── needle-api-stub/           # 上面那个脚本用的 needle-rs API 桩，不参与构建
├── docs/                          # 实机验收截图
└── README.md
```

`core` 不依赖 GUI，所以将来加一个 CLI 前端可以直接复用它。

---

## 六、常见问题

**界面上的中文全是方块**
说明系统里没找到中文字体。程序会在 `C:\Windows\Fonts\`（Windows）或
`/System/Library/Fonts/`（macOS）里找微软雅黑 / 苹方等字体，正常情况下不会出问题。
如果出现了，说明系统字体目录被动过。日志区会有一行警告。

**提示「未找到 ffmpeg」**
装好 ffmpeg 后，在界面的「ffmpeg 目录」里填上 `bin` 目录的完整路径，点「重新检查工具」。
注意要 `ffmpeg` 和 `ffprobe` 两个都在。

**报「跨集比对至少需要 2 集」**
跨集比对的原理是「找出两集里一模一样的音频片段」，所以至少得有两集。只有一集时
请手动填片头片尾时间戳。

**检测跑完，但一集都没找到片头**
先确认这些剧集**本来就有**公共片头（很多美剧第一集没有）。真有的话，
按上面「第 3 点」调参数，或者勾「强制重新分析」清掉可能过期的缓存。

**切割中间想取消**
日志区右上角的「取消当前任务」。子进程会被直接杀掉。
（注意：用库直链后端时取消只能在一个阶段结束后生效。）

**想留着 needle 的缓存，下次跑快点**
勾选「保留 needle 生成的临时文件」。默认会清掉这些工作产物，而且只删本次运行
确实新建的那些 —— 你自己手工跑 needle 留下的缓存不会被动。

---

## 七、许可

MIT。
