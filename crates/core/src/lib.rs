//! # intro-outro-core
//!
//! 电视剧片头片尾批量去除工具的**核心引擎**。不依赖任何 GUI 库，
//! 可以被 GUI、未来的 CLI 或集成测试直接复用。
//!
//! ## 一条主线
//!
//! ```text
//! scan_videos      扫描 / 拖进来的目录，收集视频文件
//!      ↓
//! probe_all        用 ffprobe 读时长、分辨率、有没有音轨
//!      ↓
//! CliDetector      让 needle 找出每集的公共片头 / 片尾时间戳
//! LibraryDetector   （同上的库后端，需 --features needle-lib）
//!      ↓
//! build_tasks      合成切割计划：算出要保留哪几段、有没有问题
//!      ↓
//! Cutter           用 ffmpeg 切，输出到**新目录**，源文件不动
//! ```
//!
//! ## 依赖的外部工具
//!
//! | 工具 | 用途 | 必需性 |
//! |---|---|---|
//! | `ffmpeg` / `ffprobe` | 探测媒体信息、切割、抽帧预览 | **必需** |
//! | `needle` | 跨集比对找片头片尾 | 走默认的 CLI 后端时必需 |
//!
//! 两者都在 `PATH` 里，或者由调用方显式指定所在目录。
//!
//! ## 为什么默认是子进程调 needle，而不是链接 needle-rs
//!
//! | | 子进程（默认） | 链接 needle-rs |
//! |---|---|---|
//! | 构建时间 | 几十秒，无原生依赖 | 需要 FFmpeg 开发库；Windows 上走 vcpkg 编 FFmpeg，以小时计 |
//! | 运行依赖 | 一个约 7 MB 的 `needle` 可执行文件 | 无 |
//! | 逐集进度 | **有**（逐集调用，自己控制） | 无（库调用是黑箱） |
//! | 可取消 | 能直接 kill 子进程 | 只能等一个阶段跑完 |
//! | 源目录残留 | 会短暂出现 `.needle.dat`，跑完自动清 | 无 |
//!
//! 最后两行是关键：needle 作为库时既不能中途取消、也不回报进度，因为
//! `Analyzer::run` 和 `Comparator::run_with_frame_hashes` 都是同步阻塞且
//! 没有任何回调的。对动辄几分钟的季度批处理来说，能看着进度、能点取消
//! 比省下一个 7 MB 的可执行文件重要得多。
//!
//! ## 上游 needle 的两个坑（本引擎的设计就是绕开它们）
//!
//! ### 1. `needle search --analyze` 永远搜不到片尾
//!
//! `Comparator::run(analyze = true, ...)` 内部用的是
//! `Analyzer::<&Path>::default().with_force(true)`，而 `Analyzer::default()`
//! 的 `include_endings` 是 **`false`**。`run_single` 就是看这个字段决定要不要
//! 算片尾数据的。所以这条路算出来的帧哈希里**根本没有片尾数据**，
//! `--include-endings` 传不传都一样 —— 它只影响搜索逻辑，改变不了输入。
//!
//! 正确做法是两步：先 `needle analyze --include-endings` 把片尾数据写进
//! `.needle.dat`，再跑**不带** `--analyze` 的 `needle search` 去读它。
//! [`detect::cli::CliDetector`] 就是这么实现的。
//!
//! ### 2. `SearchResult` 拿不出来
//!
//! `needle::audio::SearchResult` 的 `opening` / `ending` 字段是私有的，
//! 也没有任何 getter。它自己的 CLI 能显示结果，是因为它传了 `display = true`
//! 让库直接 `println!` 到 stdout —— `main.rs` 里 `comparator.run(...)?` 的
//! 返回值是被丢掉的。所以想要结构化数据，只能让它写 JSON 旁挂文件再读回来
//! （见 [`skipfile`]）。
//!
//! ### 附带一提：上游文档里的示例代码编译不过
//!
//! `needle/src/lib.rs` 开头的文档注释写的是
//! `analyzer.run(1.0, 3.0, false, true)`，四个参数。真实签名是
//! `run(hash_duration: Duration, persist: bool, threading: bool)`，三个参数，
//! 而且第一参是 `Duration` 不是秒数。该 crate 的 `Cargo.toml` 里设了
//! `doctest = false`，这段示例从来没被编译检查过，所以一直没人发现。
//! 照抄会直接编译失败。
//!
//! ## 快速上手
//!
//! ```no_run
//! use std::path::PathBuf;
//! use intro_outro_core::detect::{CliDetector, DetectOptions, new_cancel_flag};
//! use intro_outro_core::pipeline;
//! use intro_outro_core::probe::FfmpegTools;
//!
//! # fn main() -> intro_outro_core::Result<()> {
//! let tools = FfmpegTools::discover(None)?;
//! let detector = CliDetector::discover(None)?;
//! let cancel = new_cancel_flag();
//!
//! // 1. 收集文件
//! let videos = pipeline::scan_videos(&[PathBuf::from("./Season 01")], false)?;
//!
//! // 2. 探测
//! let files = pipeline::probe_all(&tools, &videos, &cancel, &mut |_, _, _| {})?;
//!
//! // 3. 检测
//! let opts = DetectOptions::default();
//! let detections = detector.detect(&videos, &opts, &cancel, &mut |ev| {
//!     println!("{ev:?}");
//! })?;
//!
//! // 4. 出计划
//! let tasks = pipeline::build_tasks(&files, &detections, &PathBuf::from("./out"), "_trimmed");
//! println!("{} 集待切割", tasks.iter().filter(|t| t.kind.is_actionable()).count());
//! # Ok(())
//! # }
//! ```

pub mod cut;
pub mod detect;
pub mod error;
pub mod exec;
pub mod model;
pub mod pipeline;
pub mod probe;
pub mod skipfile;

pub use cut::{CutMode, CutObserver, CutOutcome, CutRequest, Cutter, NoopObserver};
pub use detect::{new_cancel_flag, CancelFlag, CliDetector, DetectEvent, DetectOptions};
pub use error::{CoreError, Result};
pub use model::{
    format_clock, format_timestamp, keep_segments, parse_timestamp, Detection, EpisodeFile,
    EpisodeTask, Segment, TaskKind,
};
pub use pipeline::{CutSink, CutSummary};
pub use probe::{FfmpegTools, MediaInfo};

#[cfg(feature = "needle-lib")]
pub use detect::LibraryDetector;
