//! 检测引擎：把一批视频交给 needle，拿回每集的片头 / 片尾时间戳。
//!
//! 提供两个可互换的后端：
//!
//! | 后端 | 开启方式 | 构建代价 | 运行时依赖 | 备注 |
//! |---|---|---|---|---|
//! | [`CliDetector`] | 默认 | 无原生依赖，几十秒 | 一个 `needle` 可执行文件 | 能用官方 release 的预编译版 |
//! | [`LibraryDetector`] | `--features needle-lib` | 要 FFmpeg 开发库；Windows 上走 vcpkg 编 FFmpeg，以小时计 | 无 | 不需要 `.needle.dat` 旁挂文件 |
//!
//! 两者的产出完全一样（都是 `Vec<Detection>`，按输入顺序对齐），上层不必关心
//! 用的是哪个。
//!
//! # 几个上游行为，是本模块设计的直接原因
//!
//! ## 1. 官方 release 的二进制和 GitHub main 的 CLI 不一致
//!
//! 这条是最容易被绊倒的。实测官方 `needle-v0.1.5-windows-amd64.zip`：
//! `analyze` 接受 `--mode / --hash-period / --hash-duration / --threaded-decoding / --force`，
//! **没有** `--include-endings`；而 GitHub main（`1e6f92e`）的 `analyze` **有**
//! `--include-endings`，反而去掉了 `--hash-period`。`search` 一侧也各有各的差异
//! （v0.1.5 有 `--openings-only`，main 有 `--include-endings`）。
//!
//! 根因在库层：v0.1.5 的 `Analyzer` 对整段音频做哈希（片尾天然包含在内，不需要开关），
//! main 为了提速改成默认只哈希开头一小段，片尾必须显式开启。
//!
//! 传错参数的报错藏得很深 —— clap 非零退出，界面上只会显示「一集都没分析成功」。
//! 所以 CLI 后端**在跑之前先读 `--help` 探测**（[`cli::CliCapabilities`]），
//! 按实际支持情况拼命令，两个版本都能跑。
//!
//! ## 2. 不用 `needle search --analyze`
//!
//! `Comparator::run(analyze = true, ...)` 内部这样构造分析器：
//!
//! ```ignore
//! let analyzer = Analyzer::<&Path>::default().with_force(true);
//! ```
//!
//! `with_force(true)` 意味着它**无视**磁盘上已有的 `.needle.dat`，当场重算。
//! 于是这一步既是不可取消、没有逐集进度的黑盒，又把我们刚逐集算好的缓存白扔了。
//! （在 main 上还多一层问题：`Analyzer::default()` 的 `include_endings` 是 `false`，
//! 重算出来的哈希根本不含片尾数据。）
//!
//! 结论：**必须**先用独立的 `needle analyze` 逐集落盘，再跑不带 `--analyze` 的
//! `needle search` 去读它。本模块的 CLI 后端就是按这个两步流程实现的。
//!
//! ## 3. needle 不提供逐集进度
//!
//! needle 内部用 rayon 按「文件 × 文件」的配对并行，跑的过程中一个字都不输出，
//! 最后一次性打印结果。想从它嘴里问出「现在分析到第几集了」是不可能的。
//!
//! 所以 CLI 后端把慢的那一步（解码 + 重采样 + 指纹，约占 90% 时间）拆成
//! **逐集调用 `needle analyze`**，自己用一个小线程池并发跑，每完成一集就报一次
//! 进度。这样既有真实进度，又保住了跨集并行（池里 N 个进程并行）；
//! 快的比对那一步仍然交给 needle 一次性跑完。
//!
//! ## 4. 逐集调用顺手挡下了上游的一个 panic
//!
//! `Analyzer::run_single` 复用磁盘缓存那一段写的是
//! `bincode::deserialize_from(&f).unwrap()` —— 只要 `.needle.dat` 损坏或被截断
//! （进程被 kill、磁盘写满都会造成），needle 会**直接 panic**。
//! 若把整季塞给一个进程，一份坏缓存就让整批检测全废；逐集调用把爆炸半径
//! 限制在一集以内：那一集报失败，其余照常出结果。
//!
//! ## 5. 结果只能靠 skip file 传出来
//!
//! `SearchResult` 的 `opening` / `ending` 字段是**私有**的，也没有任何 getter ——
//! 库作者只在 `display = true` 时把它们 `println!` 到 stdout，返回值本身取不出内容。
//! 所以结构化数据只能靠 `--write-skip-files` 落盘再读回来（见 [`crate::skipfile`]）。
//!
//! ## 6. 缓存复用只比对视频头部 MD5，不管缓存是怎么生成的
//!
//! 缓存命中时 needle 会打印 `Skipping analysis for <文件>...`，这行会出现在
//! 我们的日志里，用户能直观看到第二次跑快在哪。但因为它只比对 MD5，
//! **用「只检测片头」生成的缓存会被原样复用** —— 里面没有片尾数据。
//! 这正是 [`DetectOptions::force_reanalyze`] 存在的理由。

pub mod cli;
#[cfg(feature = "needle-lib")]
pub mod needle_lib;

pub use cli::{CliCapabilities, CliDetector};
#[cfg(feature = "needle-lib")]
pub use needle_lib::LibraryDetector;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{CoreError, Result};
use crate::model::Detection;
use crate::skipfile;

/// 检测过程中回报给界面的事件。
#[derive(Debug, Clone)]
pub enum DetectEvent {
    /// 进入一个新阶段。文案会直接显示给用户，所以要写人话。
    Stage(String),
    /// 被调用工具的原始输出，一行一条。
    Log(String),
    /// 一集分析完成。
    EpisodeDone {
        index: usize,
        total: usize,
        name: String,
    },
    /// 一集分析失败。不中断整轮检测，继续跑剩下的 —— 一集读不了不该让用户
    /// 重跑整个季度。
    EpisodeFailed {
        index: usize,
        total: usize,
        name: String,
        error: String,
    },
}

/// 检测参数。
#[derive(Debug, Clone)]
pub struct DetectOptions {
    /// 是否检测片尾。关掉能省掉一遍完整的解码（片尾要额外处理视频最后一段）。
    pub include_endings: bool,
    /// 哈希匹配阈值，0（完全相同）～ 32（完全不同）。越小越严格。
    pub hash_match_threshold: u32,
    /// 片头最短时长（秒）。设成接近真实片头长度能显著减少误报。
    pub min_opening_duration_secs: u32,
    /// 片尾最短时长（秒）。
    pub min_ending_duration_secs: u32,
    /// 时间边距（秒）：片头起点往后、片尾终点往前各收这么多。
    /// 用来抵消检测误差，保证不误删正片内容。
    pub time_padding_secs: f64,
    /// 是否启用多线程。
    pub threading: bool,
    /// 强制重新分析，忽略已有的 `.needle.dat` 缓存。
    ///
    /// 需要它的场景很具体：如果之前用「只检测片头」跑过一次，缓存里的帧哈希
    /// 没有片尾数据；再打开片尾检测时，needle 会直接复用那份不含片尾的缓存，
    /// 结果又是「搜不到片尾」。勾上这个强制重算。
    pub force_reanalyze: bool,
    /// 是否保留 needle 写出的旁挂文件（`.needle.dat` / `.needle.skip.json`）。
    ///
    /// 默认 `false`：这些文件是我们让 needle 生成的工作产物，跑完就该清掉，
    /// 不该在用户辛辛苦苦整理好的剧集目录里留一堆垃圾。清理时只删「本次运行前
    /// 不存在」的那些，用户自己原有的缓存不会被动。
    pub keep_sidecars: bool,
    /// 逐集并行分析的并发数。
    pub analyze_parallelism: usize,
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self {
            include_endings: true,
            // 以下是 needle 自己的默认值，保持一致以免用户困惑
            hash_match_threshold: 10,
            min_opening_duration_secs: 20,
            min_ending_duration_secs: 20,
            time_padding_secs: 0.0,
            threading: true,
            force_reanalyze: false,
            keep_sidecars: false,
            analyze_parallelism: default_parallelism(),
        }
    }
}

/// 默认并发数：取 CPU 核数的一半，上限 4。
///
/// 不完全放开的理由：每个 needle 进程自己内部也会开线程，池子开满会把
/// 内存带宽吃干，反而比少开几个慢；而且这些机器通常同时在跑别的东西。
pub fn default_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).clamp(1, 4)
}

/// 取消标志。被置位后各阶段会尽快停下来并把子进程杀掉。
pub type CancelFlag = std::sync::Arc<AtomicBool>;

/// 造一个未触发的取消标志。
pub fn new_cancel_flag() -> CancelFlag {
    std::sync::Arc::new(AtomicBool::new(false))
}

/// 检查取消标志，被置位就返回 [`CoreError::Cancelled`]。
pub(crate) fn check_cancel(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Relaxed) {
        Err(CoreError::Cancelled)
    } else {
        Ok(())
    }
}

/// 列表里显示用的短名字。
pub(crate) fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// 旁挂文件的「运行前状态」快照。
///
/// 存在的意义是让清理是**精确且安全**的：只删本次运行真正新造出来的文件。
/// 用户如果之前自己手工跑过 `needle analyze`，那份 `.needle.dat` 是他的缓存，
/// 我们没有资格顺手删掉。
#[derive(Debug)]
pub(crate) struct SidecarSnapshot {
    entries: Vec<(PathBuf, bool)>,
}

impl SidecarSnapshot {
    /// 记录每个视频对应的所有旁挂文件当前存不存在。
    ///
    /// 注意用的是 [`skipfile::sidecar_paths`] 而不是「硬编码两个路径」：
    /// needle 两个版本给帧哈希文件起的名字不一样（`needle.bin` / `needle.dat`），
    /// 只有把两个都列上，清理才不会漏。
    pub(crate) fn take(files: &[PathBuf]) -> Self {
        let mut entries = Vec::new();
        for f in files {
            for path in skipfile::sidecar_paths(f) {
                let existed = path.is_file();
                entries.push((path, existed));
            }
        }
        Self { entries }
    }

    /// 删掉本次新产生的旁挂文件，返回删掉的数量。
    ///
    /// 删除失败只记日志不报错 —— 清理是收尾工作，不该把一次成功的检测变成失败。
    pub(crate) fn cleanup_new(&self, emit: &mut dyn FnMut(DetectEvent)) -> usize {
        let mut removed = 0usize;
        for (path, existed_before) in &self.entries {
            if *existed_before || !path.is_file() {
                continue;
            }
            match std::fs::remove_file(path) {
                Ok(()) => {
                    removed += 1;
                    emit(DetectEvent::Log(format!(
                        "已清理临时文件：{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )));
                }
                Err(e) => {
                    emit(DetectEvent::Log(format!(
                        "清理 {} 失败（不影响结果）：{e}",
                        path.display()
                    )));
                }
            }
        }
        removed
    }
}

/// 按输入顺序把每集的旁挂结果读出来，凑成与 `files` 等长、按下标对齐的结果。
pub(crate) fn collect_detections(
    files: &[PathBuf],
    emit: &mut dyn FnMut(DetectEvent),
) -> Result<Vec<Detection>> {
    let total = files.len();
    let mut out = Vec::with_capacity(total);
    for (i, f) in files.iter().enumerate() {
        let det = skipfile::read_detection(f)?;
        match &det {
            Some(d) => {
                let mut parts = Vec::new();
                if let Some(o) = d.opening {
                    parts.push(format!("片头 {}", o.display()));
                }
                if let Some(e) = d.ending {
                    parts.push(format!("片尾 {}", e.display()));
                }
                emit(DetectEvent::Log(format!(
                    "[{}/{}] {} -> {}",
                    i + 1,
                    total,
                    display_name(f),
                    parts.join("，")
                )));
            }
            None => emit(DetectEvent::Log(format!(
                "[{}/{}] {} -> 未检测到片头或片尾",
                i + 1,
                total,
                display_name(f)
            ))),
        }
        out.push(det.unwrap_or_default());
    }

    if out.iter().all(|d| d.is_empty()) {
        return Err(CoreError::NoDetectionAtAll);
    }
    Ok(out)
}
