//! 子进程调用 `needle` CLI 的检测后端（默认后端）。
//!
//! 选它做默认，是因为它是唯一一条**在干净机器上几十秒就能构建出来**的路：
//! 没有原生依赖、不需要 FFmpeg 开发库、不需要 cmake 或 vcpkg。
//! 代价只是运行时需要一个 `needle` 可执行文件 —— 官方 release 提供了
//! 单文件 Windows 版（约 7 MB，自带静态 FFmpeg），下载解压即用。
//!
//! # 官方二进制和 GitHub main 的 CLI 不一样（这条最坑）
//!
//! 实测：官方 release 的 `needle-v0.1.5-windows-amd64.zip` 里，`needle analyze --help`
//! 输出的是 `--mode / --hash-period / --hash-duration / --threaded-decoding / --force`；
//! 而 GitHub main 分支（2024-08 的 `1e6f92e`）的 `analyze` 多了 `--include-endings`、
//! `--opening-search-percentage`、`--ending-search-percentage`，少了 `--hash-period`。
//! 两者的 `search` 也不同：v0.1.5 有 `--openings-only`、`--opening-search-percentage`，
//! main 有 `--include-endings` 而没有前两者。
//!
//! 根因在库层：v0.1.5 的 `Analyzer` 对**整段音频**做哈希，所以不需要「要不要管片尾」
//! 这个开关；main 为了省时间把分析改成默认只哈希开头一小段，片尾就必须靠
//! `--include-endings` 显式开启。于是同一个参数在一侧存在、另一侧不存在。
//!
//! 传错参数的后果很隐蔽：clap 会以非零码退出，界面上只看到「一集都没分析成功」，
//! 而真正的错误信息混在被丢弃的 stderr 里。所以**不能猜**，必须先探测
//! （见 [`CliCapabilities`]）。
//!
//! 关于「为什么逐集调用」和「为什么不传 `--analyze`」，见 [`super`] 的文档。

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::error::{CoreError, Result};
use crate::exec::{capture, run_streaming, Stream};
use crate::model::Detection;

use super::{
    check_cancel, collect_detections, display_name, CancelFlag, DetectEvent, DetectOptions,
    SidecarSnapshot,
};

/// needle 的 `--hash-match-threshold` 是 u16，且语义上只能 0～32（0 完全相同，32 完全不同）。
const THRESHOLD_MAX: u32 = 32;
/// `--min-opening-duration` / `--min-ending-duration` 是 u16。上界收紧到 1 小时 ——
/// 超过一小时的「片头」不可能是片头，是检测出错，不该让它传下去。
const DURATION_MAX: u32 = 3600;
/// `--time-padding` 是 f32。超过 60 秒的边距会把正片切掉，同样收紧。
const PADDING_MAX: f64 = 60.0;

/// 探测出来的 needle CLI 能力。
///
/// 只有一件事需要探测，但它决定了命令能不能跑起来。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CliCapabilities {
    /// `analyze` 是否接受 `--include-endings`。
    ///
    /// `true` → 这个构建的分析器默认只哈希开头，片尾必须显式开启（main 分支）。
    /// `false` → 分析器对整段音频做哈希，片尾天然包含在内，也**不能**传这个参数（v0.1.5）。
    pub analyze_include_endings: bool,
    /// `search` 是否接受 `--include-endings`（main 分支）。
    /// v0.1.5 一侧对应的是 `--openings-only`（默认关），也就是说默认就搜片尾。
    pub search_include_endings: bool,
}

/// 通过子进程调用 needle CLI 来检测片头片尾。
#[derive(Debug, Clone)]
pub struct CliDetector {
    /// needle 可执行文件的路径。
    pub needle_exe: PathBuf,
}

impl CliDetector {
    pub fn new(needle_exe: impl Into<PathBuf>) -> Self {
        Self {
            needle_exe: needle_exe.into(),
        }
    }

    /// 在给定目录或 PATH 里找 needle。
    pub fn discover(explicit_dir: Option<&Path>) -> Result<Self> {
        let exe = crate::exec::find_executable("needle", explicit_dir)
            .ok_or(CoreError::ToolNotFound { tool: "needle" })?;
        Ok(Self::new(exe))
    }

    /// 取 `needle --version`，用于界面上确认版本。
    pub fn version(&self) -> Result<String> {
        let mut cmd = Command::new(&self.needle_exe);
        cmd.arg("--version");
        let out = capture(&mut cmd)?;
        let v = out.stdout.trim().to_string();
        if v.is_empty() {
            return Err(CoreError::ParseFailed {
                what: "needle --version",
                detail: out.stderr.trim().to_string(),
            });
        }
        Ok(v)
    }

    /// 探测这个 needle 构建支持哪些可选参数。
    ///
    /// 做法是读 `--help` 的文本 —— 比「跑一次带参数的调用看它报不报错」更轻，
    /// 而且不会有副作用。两次子进程调用合计约 50ms，每次检测跑一次即可，
    /// 不值得为它加缓存字段（那样 `CliDetector` 就不能是 `Copy`/`Clone` 的纯数据了）。
    pub fn capabilities(&self) -> CliCapabilities {
        CliCapabilities {
            analyze_include_endings: self
                .help_mentions(&["analyze", "--help"], "--include-endings"),
            search_include_endings: self.help_mentions(&["search", "--help"], "--include-endings"),
        }
    }

    /// 跑 `<子命令> --help`，看输出里有没有某个选项。
    fn help_mentions(&self, args: &[&str], flag: &str) -> bool {
        let mut cmd = Command::new(&self.needle_exe);
        cmd.args(args);
        match capture(&mut cmd) {
            Ok(out) => mentions_flag(&out.stdout, flag) || mentions_flag(&out.stderr, flag),
            // 探测本身失败时保守地按「不支持」处理：少传一个可选参数最多是行为
            // 退化成默认值；传了一个不认识的参数则是硬失败，两害相权取其轻。
            Err(e) => {
                tracing::debug!(error = %e, "探测 needle 子命令参数失败，按不支持处理");
                false
            }
        }
    }

    /// 跑完整套检测，返回与 `files` **按下标一一对应**的结果。
    ///
    /// 没检测到片头片尾的集会用 `Detection::default()`（两个字段都是 `None`）
    /// 占位，而不会被跳过 —— 否则调用方就没法把结果和文件对上了。
    pub fn detect(
        &self,
        files: &[PathBuf],
        opts: &DetectOptions,
        cancel: &CancelFlag,
        emit: &mut dyn FnMut(DetectEvent),
    ) -> Result<Vec<Detection>> {
        if files.len() < 2 {
            return Err(CoreError::NotEnoughEpisodes(files.len()));
        }
        check_cancel(cancel)?;

        // 先记录运行前哪些旁挂文件已经存在，收尾时只删本次新造的（见 SidecarSnapshot）
        let snapshot = SidecarSnapshot::take(files);

        let outcome = self.run_pipeline(files, opts, cancel, emit);

        if !opts.keep_sidecars {
            emit(DetectEvent::Stage("清理临时文件".to_string()));
            let removed = snapshot.cleanup_new(emit);
            if removed == 0 {
                emit(DetectEvent::Log("没有需要清理的临时文件".to_string()));
            }
        }

        outcome
    }

    /// 三步流程：逐集算指纹 → 一次性跨集比对 → 读回结果。
    fn run_pipeline(
        &self,
        files: &[PathBuf],
        opts: &DetectOptions,
        cancel: &CancelFlag,
        emit: &mut dyn FnMut(DetectEvent),
    ) -> Result<Vec<Detection>> {
        let caps = self.capabilities();
        emit(DetectEvent::Log(format!(
            "needle 参数探测：analyze{}支持 --include-endings，search{}支持",
            if caps.analyze_include_endings {
                ""
            } else {
                "不"
            },
            if caps.search_include_endings {
                ""
            } else {
                "不"
            }
        )));
        if opts.include_endings && !caps.analyze_include_endings {
            // v0.1.5 这一侧：分析器本来就把整段音频都做了哈希，片尾不需要额外开关
            emit(DetectEvent::Log(
                "这个 needle 构建的分析器会对整段音频做哈希，片尾天然包含在内，无需额外开关"
                    .to_string(),
            ));
        }

        emit(DetectEvent::Stage(if opts.include_endings {
            "分析音频指纹（含片尾片段）".to_string()
        } else {
            "分析音频指纹".to_string()
        }));
        emit(DetectEvent::Log(format!(
            "共 {} 个文件，并发 {} 路逐集分析",
            files.len(),
            opts.analyze_parallelism.max(1).min(files.len())
        )));
        if !opts.keep_sidecars && !opts.force_reanalyze {
            emit(DetectEvent::Log(
                "本轮跑完会清掉 .needle.dat 以保持源目录干净，代价是下次要重新分析；\
                 想复用缓存（第二次跑快很多）就勾上「保留 needle 生成的临时文件」。"
                    .to_string(),
            ));
        }

        let analyzed = self.analyze_each(files, opts, caps, cancel, emit)?;
        check_cancel(cancel)?;

        if analyzed.len() < 2 {
            // 大多数集都分析失败时，继续跑比对没有意义，直接报清楚
            return Err(CoreError::NotEnoughEpisodes(analyzed.len()));
        }
        if analyzed.len() < files.len() {
            emit(DetectEvent::Log(format!(
                "{} 个文件分析失败已跳过，用剩下 {} 个继续比对",
                files.len() - analyzed.len(),
                analyzed.len()
            )));
        }

        emit(DetectEvent::Stage(
            "跨集比对，搜索公共片头 / 片尾".to_string(),
        ));
        emit(DetectEvent::Log(
            "这一步由 needle 内部整体并行完成，过程中不会输出进度，请稍候".to_string(),
        ));
        self.search(&analyzed, opts, caps, cancel, emit)?;

        check_cancel(cancel)?;
        emit(DetectEvent::Stage("读取检测结果".to_string()));
        collect_detections(files, emit)
    }

    /// 逐集并行跑 `needle analyze`，边跑边报进度。
    ///
    /// 返回成功分析的文件（顺序与输入一致），失败的那些已经通过
    /// [`DetectEvent::EpisodeFailed`] 报上去了。
    fn analyze_each(
        &self,
        files: &[PathBuf],
        opts: &DetectOptions,
        caps: CliCapabilities,
        cancel: &CancelFlag,
        emit: &mut dyn FnMut(DetectEvent),
    ) -> Result<Vec<PathBuf>> {
        let total = files.len();
        let parallelism = opts.analyze_parallelism.max(1).min(total);

        // 用一个共享队列分发下标，让先干完的线程接着领下一个 ——
        // 比把文件平均切片更好，因为各集时长不一，切片会出现长尾。
        let queue = Arc::new(Mutex::new((0..total).collect::<VecDeque<usize>>()));
        let (tx, rx) = mpsc::channel::<WorkerMsg>();

        let mut handles = Vec::with_capacity(parallelism);
        for _ in 0..parallelism {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            let cancel = Arc::clone(cancel);
            let exe = self.needle_exe.clone();
            let opts = opts.clone();
            let files: Vec<PathBuf> = files.to_vec();

            handles.push(thread::spawn(move || loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let next = match queue.lock() {
                    Ok(mut q) => q.pop_front(),
                    // 锁中毒意味着另一个线程 panic 了，继续跑没有意义
                    Err(_) => break,
                };
                let Some(index) = next else { break };

                let video = files[index].clone();
                let error = run_analyze_one(&exe, &video, &opts, caps, &cancel, &tx)
                    .err()
                    .map(|e| e.to_string());

                if tx.send(WorkerMsg::Done { index, error }).is_err() {
                    break;
                }
            }));
        }
        // 关键：主线程自己持有的那个发送端必须 drop，否则下面的 rx 永远不结束
        drop(tx);

        let mut failed: HashSet<usize> = HashSet::new();
        let mut finished = 0usize;
        for msg in rx {
            match msg {
                WorkerMsg::Line(line) => emit(DetectEvent::Log(line)),
                WorkerMsg::Done { index, error } => {
                    finished += 1;
                    let name = display_name(&files[index]);
                    match error {
                        None => emit(DetectEvent::EpisodeDone {
                            index: finished,
                            total,
                            name,
                        }),
                        Some(error) => {
                            failed.insert(index);
                            emit(DetectEvent::EpisodeFailed {
                                index: finished,
                                total,
                                name,
                                error,
                            });
                        }
                    }
                }
            }
        }

        for h in handles {
            // 线程里如果 panic 了，join 会返回 Err；这里吞掉是有意的：
            // panic 的影响已经反映在「少了哪一集的结果」上，不需要再炸一次
            let _ = h.join();
        }

        check_cancel(cancel)?;

        Ok(files
            .iter()
            .enumerate()
            .filter(|(i, _)| !failed.contains(i))
            .map(|(_, p)| p.clone())
            .collect())
    }

    /// 一次性跑 `needle search`，让它算出结果并写成 skip file。
    fn search(
        &self,
        files: &[PathBuf],
        opts: &DetectOptions,
        caps: CliCapabilities,
        cancel: &CancelFlag,
        emit: &mut dyn FnMut(DetectEvent),
    ) -> Result<()> {
        let mut cmd = self.base_command(opts);
        cmd.arg("search")
            // 必须有这个：结果只能通过它落到磁盘上（见 skipfile 模块文档）
            .arg("--write-skip-files")
            .arg("--hash-match-threshold")
            .arg(clamp_threshold(opts.hash_match_threshold).to_string())
            .arg("--min-opening-duration")
            .arg(clamp_duration(opts.min_opening_duration_secs).to_string())
            .arg("--min-ending-duration")
            .arg(clamp_duration(opts.min_ending_duration_secs).to_string())
            .arg("--time-padding")
            .arg(format!("{:.3}", clamp_padding(opts.time_padding_secs)));

        if opts.include_endings && caps.search_include_endings {
            cmd.arg("--include-endings");
        }
        // 绝对不要传 --analyze：needle 在这条路径里用 `Analyzer::default().with_force(true)`
        // 现算一遍指纹 —— 既会**无视**我们刚逐集算好并落盘的 `.needle.dat`，
        // 又把这一步变成不可取消、没有逐集进度的黑盒。详见模块文档与父模块文档。

        // `--` 之后一律当位置参数：剧集文件名以 '-' 开头时不会被误认成选项
        cmd.arg("--");
        cmd.args(files);

        let mut log = |_stream: Stream, line: &str| {
            let line = line.trim();
            if !line.is_empty() {
                emit(DetectEvent::Log(line.to_string()));
            }
        };
        let res = run_streaming(&mut cmd, Some(Arc::clone(cancel)), &mut log)?;
        if res.cancelled {
            return Err(CoreError::Cancelled);
        }
        res.into_error("needle search")?;
        // needle 在「一个都没匹配上」时也是 0 退出码，所以退出码不能当成功判据，
        // 真正的判据在 collect_detections 里（全部为空则报 NoDetectionAtAll）
        Ok(())
    }

    /// 构造带全局标志的基础命令。
    ///
    /// `--no-threading` 是 needle 顶层（而不是子命令）的选项，clap 要求它出现在
    /// 子命令**之前**。放错位置会直接报「unexpected argument」。
    fn base_command(&self, opts: &DetectOptions) -> Command {
        let mut cmd = Command::new(&self.needle_exe);
        if !opts.threading {
            cmd.arg("--no-threading");
        }
        cmd
    }
}

/// 工作线程往主线程回传的消息。
enum WorkerMsg {
    /// 被调用工具的一行输出
    Line(String),
    /// 一集跑完了，`error` 为 `None` 表示成功
    Done { index: usize, error: Option<String> },
}

/// 对一个视频跑 `needle analyze`，把它的输出转发回主线程。
fn run_analyze_one(
    exe: &Path,
    video: &Path,
    opts: &DetectOptions,
    caps: CliCapabilities,
    cancel: &CancelFlag,
    tx: &mpsc::Sender<WorkerMsg>,
) -> Result<()> {
    let mut cmd = Command::new(exe);
    if !opts.threading {
        cmd.arg("--no-threading");
    }
    cmd.arg("analyze");
    if opts.include_endings && caps.analyze_include_endings {
        // main 分支上这个开关决定片尾能不能被检测到；v0.1.5 没有它（也不需要有）
        cmd.arg("--include-endings");
    }
    if opts.force_reanalyze {
        cmd.arg("--force");
    }
    cmd.arg("--").arg(video);

    let name = display_name(video);
    let mut on_line = |_stream: Stream, line: &str| {
        let line = line.trim();
        if !line.is_empty() {
            // 多路并发时行会交错，前缀标出是哪一集，否则日志没法读
            let _ = tx.send(WorkerMsg::Line(format!("[{name}] {line}")));
        }
    };

    let res = run_streaming(&mut cmd, Some(Arc::clone(cancel)), &mut on_line)?;
    if res.cancelled {
        return Err(CoreError::Cancelled);
    }
    res.into_error(&format!("needle analyze {}", video.display()))?;

    // needle 分析成功后不打印任何东西，所以「没报错」等于「有结果」。
    // 但有个边界：如果视频没有音轨，它可能在 stderr 上只留一句警告就退 0。
    // 这种情况留给后面 search 阶段去暴露（比对时那些集自然匹配不上）。
    Ok(())
}

/// 帮助文本里有没有出现某个选项。
///
/// 单独抽出来是为了能对「选项名被行宽折断」这类情形有明确行为 ——
/// clap 只会折行到选项名之后的描述文字，选项名本身不会被切开，
/// 所以直接子串匹配是安全的。
fn mentions_flag(help_text: &str, flag: &str) -> bool {
    help_text.contains(flag)
}

fn clamp_threshold(v: u32) -> u32 {
    v.min(THRESHOLD_MAX)
}

fn clamp_duration(v: u32) -> u32 {
    v.clamp(1, DURATION_MAX)
}

fn clamp_padding(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, PADDING_MAX)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_out_of_range_parameters_before_handing_them_to_needle() {
        // needle 的 u16 参数解析一旦失败就是直接退出，不能把越界值传下去
        assert_eq!(clamp_threshold(999), 32);
        assert_eq!(clamp_threshold(10), 10);
        assert_eq!(clamp_duration(0), 1);
        assert_eq!(clamp_duration(20), 20);
        assert_eq!(clamp_duration(99_999), 3600);
        assert_eq!(clamp_padding(-5.0), 0.0);
        assert_eq!(clamp_padding(1e9), 60.0);
        assert_eq!(clamp_padding(f64::NAN), 0.0);
    }

    /// 真实的两个版本 `analyze --help` 片段（照抄实测输出，只保留选项名那一行）。
    /// 这两个样本锁住「探测必须按版本走」这件事：同一段代码要能认出该不该传参数。
    #[test]
    fn detects_version_drift_between_released_binary_and_main() {
        let v015_analyze = "USAGE:\n    needle.exe analyze [OPTIONS] <PATHS>...\n\n\
             OPTIONS:\n        --file-headers-only\n        --force\n        --hash-duration <HASH_DURATION>\n        --hash-period <HASH_PERIOD>\n        -m, --mode <MODE>\n        --no-threading\n        --threaded-decoding\n";
        let main_analyze = "USAGE:\n    needle analyze [OPTIONS] <PATHS>...\n\n\
             OPTIONS:\n        --include-endings\n        --ending-search-percentage <ENDING_SEARCH_PERCENTAGE>\n        --hash-duration <HASH_DURATION>\n        --threaded-decoding\n";

        assert!(!mentions_flag(v015_analyze, "--include-endings"));
        assert!(mentions_flag(main_analyze, "--include-endings"));
    }

    /// 全角/中文帮助文本里也不能误判成支持。
    #[test]
    fn does_not_report_support_for_missing_flags() {
        assert!(!mentions_flag("", "--include-endings"));
        assert!(!mentions_flag(
            "--openings-only  If set, needle will only search for openings.",
            "--include-endings"
        ));
    }
}
