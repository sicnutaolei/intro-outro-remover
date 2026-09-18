//! 后台线程与界面之间的消息通道。
//!
//! # 线程模型
//!
//! 所有耗时工作（探测、检测、切割、抽帧）都跑在独立线程里，通过
//! `std::sync::mpsc` 把消息发回 UI 线程。UI 线程每帧 `try_recv` 把消息
//! 全部取干净再重绘。
//!
//! **全程序只有一条通道**：由 [`message_channel`] 建出来，`Receiver` 跟着 `App`
//! 活到底，每个后台任务只拿一个 `Sender` 克隆。所以 `spawn_*` 都不返回 `Receiver` ——
//! 这是刻意的，理由见 [`message_channel`] 的文档：早先「一件任务一条通道」的写法
//! 会把上一条通道的接收端丢掉，被丢那条线对应的界面状态就永远等不到人来清。
//!
//! **一个不能漏的细节**：后台线程每次发完消息都要调一次
//! `egui::Context::request_repaint()`。egui 是事件驱动的 —— 没有输入事件就不会
//! 重绘。少了这一句，进度会一直卡在初始状态，直到用户碰一下鼠标才突然跳到最新
//! 值，看起来就像程序卡死了。`egui::Context` 是 `Clone + Send` 的，克隆一份
//! 传进线程即可。
//!
//! # 为什么不用 `crossbeam-channel`
//!
//! 标准库的 `mpsc` 完全够用：这里只有「多生产者 → 单消费者」一种拓扑，
//! 而且消息量很小（最多每秒几条）。少一个依赖就少一处编译期风险。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use intro_outro_core::cut::{CutMode, CutOutcome, Cutter};
use intro_outro_core::detect::{CancelFlag, CliDetector, DetectEvent, DetectOptions};
use intro_outro_core::model::{Detection, EpisodeFile, EpisodeTask};
use intro_outro_core::pipeline::{self, CutSink, CutSummary};
use intro_outro_core::probe::FfmpegTools;
use intro_outro_core::Result as CoreResult;

#[cfg(feature = "needle-lib")]
use intro_outro_core::detect::LibraryDetector;

/// 检测后端的运行时选择。
#[derive(Debug, Clone)]
pub enum DetectorChoice {
    /// 子进程调用 needle 可执行文件（默认）
    Cli(CliDetector),
    /// 直接链接 needle-rs（需编译时开 `needle-lib`）
    #[cfg(feature = "needle-lib")]
    Library,
}

impl DetectorChoice {
    pub fn label(&self) -> &'static str {
        match self {
            DetectorChoice::Cli(_) => "needle 子进程",
            #[cfg(feature = "needle-lib")]
            DetectorChoice::Library => "needle-rs 库直链",
        }
    }

    /// 详细描述，包含具体可执行文件路径。
    ///
    /// 用 `match` 而不是 `if let`：只有 Cli 一个变体时（没开 needle-lib），
    /// `if let` 会触发 `irrefutable_let_patterns` 警告。
    pub fn describe(&self) -> String {
        match self {
            DetectorChoice::Cli(d) => format!("needle 子进程（{}）", d.needle_exe.display()),
            #[cfg(feature = "needle-lib")]
            DetectorChoice::Library => "needle-rs 库直链（不产生 .needle.dat）".to_string(),
        }
    }

    /// 有没有逐集进度和即时取消能力。
    pub fn has_fine_grained_progress(&self) -> bool {
        match self {
            DetectorChoice::Cli(_) => true,
            #[cfg(feature = "needle-lib")]
            DetectorChoice::Library => false,
        }
    }

    /// 跑检测。两个后端在这里被抹平成同一个签名，界面不需要知道底下是哪种。
    pub fn detect(
        &self,
        files: &[PathBuf],
        opts: &DetectOptions,
        cancel: &CancelFlag,
        emit: &mut dyn FnMut(DetectEvent),
    ) -> CoreResult<Vec<Detection>> {
        match self {
            DetectorChoice::Cli(d) => d.detect(files, opts, cancel, emit),
            #[cfg(feature = "needle-lib")]
            DetectorChoice::Library => LibraryDetector::new().detect(files, opts, cancel, emit),
        }
    }
}

/// 根据工具检查结果挑一个检测后端。
///
/// 优先级：用户显式要求库后端 → CLI 后端（能找到 needle 的话）→ 库后端（编译进来了的话）
/// → 都没有就是 `None`。
///
/// 默认偏向 CLI 后端，因为它能回报逐集进度、能即时取消，这两点对动辄几分钟的
/// 季度批处理比什么都重要。库后端只是「没能找到 needle 时的退路」。
pub fn pick_detector(needle: Option<&PathBuf>, prefer_library: bool) -> Option<DetectorChoice> {
    #[cfg(feature = "needle-lib")]
    {
        if prefer_library {
            return Some(DetectorChoice::Library);
        }
    }
    #[cfg(not(feature = "needle-lib"))]
    {
        // 没开这个 feature 时这个参数没有意义，显式忽略掉以免触发未使用告警
        let _ = prefer_library;
    }

    if let Some(p) = needle {
        return Some(DetectorChoice::Cli(CliDetector::new(p.clone())));
    }

    #[cfg(feature = "needle-lib")]
    let fallback = Some(DetectorChoice::Library);
    #[cfg(not(feature = "needle-lib"))]
    let fallback = None;
    fallback
}

/// 外部工具的检测结果。
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub ffmpeg: PathBuf,
    pub ffmpeg_version: String,
    pub needle: Option<PathBuf>,
    pub needle_version: Option<String>,
}

/// 单集切割成功后的摘要。
#[derive(Debug, Clone)]
pub struct CutInfo {
    pub output: PathBuf,
    pub kept_secs: f64,
}

/// 后台线程发回界面的消息。
#[derive(Debug)]
pub enum WorkerMessage {
    /// 工具检查完成。`Err` 里是给用户看的中文说明。
    ToolsChecked(Box<std::result::Result<ToolInfo, String>>),

    ProbeProgress {
        current: usize,
        total: usize,
        name: String,
    },
    ProbeDone(std::result::Result<Vec<EpisodeFile>, String>),

    /// 检测过程事件，直接转发给界面日志与进度
    Detect(DetectEvent),
    DetectDone(std::result::Result<Vec<Detection>, String>),

    CutLine {
        index: usize,
        line: String,
    },
    CutProgress {
        index: usize,
        fraction: f32,
    },
    CutFinished {
        index: usize,
        result: std::result::Result<CutInfo, String>,
    },
    CutAllDone(CutSummary),

    /// 抽帧预览好了，`png` 是临时 PNG 的路径
    PreviewReady {
        index: usize,
        png: PathBuf,
        at_secs: f64,
    },
    PreviewFailed {
        index: usize,
        error: String,
    },
}

/// 建一对通道，顺带把发送端的克隆给闭包用。
fn channel() -> (Sender<WorkerMessage>, Receiver<WorkerMessage>) {
    mpsc::channel()
}

/// 建一条**长命**的消息通道：`Receiver` 由界面一直持有，`Sender` 克隆给每个后台任务。
///
/// # 为什么不是「每个任务各建一条通道」
///
/// 早先的写法是每个 `spawn_*` 各自 `channel()` 并把 `Receiver` 交回界面，界面存进
/// 一个 `Option<Receiver>` 槽里 —— 于是**每起一个新任务就把上一个任务的通道丢掉**。
/// 丢掉的后果不是「老任务安静地停下」，而是：
///
/// * 那个线程仍在跑，发消息时 `send` 失败（接收端没了），错误被 `let _ =` 吞掉；
/// * 界面上对应的状态没人来清 —— 比如某一集的「预览」按钮会一直停在转圈状态，
///   从此永久禁用，只能重新探测才恢复。
///
/// 抽帧预览是最容易撞上的：它不占 `busy` 标志，点完第 1 集再点第 2 集，
/// 两次之间只隔几十上百毫秒。
///
/// 现在只有这一条通道、且 `Receiver` 永不丢弃，消息就不可能丢；拓扑仍然是
/// 模块文档里写的那样「多生产者 → 单消费者」，只是终于名副其实了。
pub fn message_channel() -> (Sender<WorkerMessage>, Receiver<WorkerMessage>) {
    channel()
}

/// 检查 ffmpeg / ffprobe / needle 是否可用，并取回版本号。
///
/// 放在后台线程里跑：这三个检查都要 spawn 进程，在启动路径上同步跑会让窗口
/// 出现之前卡顿一下，用户会以为程序启动慢。
pub fn spawn_check_tools(
    ctx: egui::Context,
    ffmpeg_dir: Option<PathBuf>,
    needle_dir: Option<PathBuf>,
    tx: Sender<WorkerMessage>,
) {
    thread::spawn(move || {
        let result = (|| -> CoreResult<ToolInfo> {
            let tools = FfmpegTools::discover(ffmpeg_dir.as_deref())?;
            let ffmpeg_version = tools.version().unwrap_or_else(|_| "版本未知".to_string());

            // needle 缺失不算致命：用户可以只做切割，或者手动填时间戳。
            // 所以这里失败只记 None，不往上抛。
            let (needle, needle_version) = match CliDetector::discover(needle_dir.as_deref()) {
                Ok(d) => {
                    let v = d.version().ok();
                    tracing::info!(path = %d.needle_exe.display(), version = ?v, "找到 needle");
                    (Some(d.needle_exe), v)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "没找到 needle");
                    (None, None)
                }
            };

            Ok(ToolInfo {
                ffmpeg: tools.ffmpeg,
                ffmpeg_version,
                needle,
                needle_version,
            })
        })()
        .map_err(|e| e.to_string());

        let _ = tx.send(WorkerMessage::ToolsChecked(Box::new(result)));
        ctx.request_repaint();
    });
}

/// 后台探测一批视频的媒体信息。
pub fn spawn_probe(
    ctx: egui::Context,
    tools: FfmpegTools,
    paths: Vec<PathBuf>,
    cancel: CancelFlag,
    tx: Sender<WorkerMessage>,
) {
    thread::spawn(move || {
        let tx_progress = tx.clone();
        let ctx_progress = ctx.clone();
        let mut on_progress = |current: usize, total: usize, name: &str| {
            let _ = tx_progress.send(WorkerMessage::ProbeProgress {
                current,
                total,
                name: name.to_string(),
            });
            ctx_progress.request_repaint();
        };

        let result = pipeline::probe_all(&tools, &paths, &cancel, &mut on_progress)
            .map_err(|e| e.to_string());

        let _ = tx.send(WorkerMessage::ProbeDone(result));
        ctx.request_repaint();
    });
}

/// 后台跑检测。
pub fn spawn_detect(
    ctx: egui::Context,
    detector: DetectorChoice,
    files: Vec<PathBuf>,
    opts: DetectOptions,
    cancel: CancelFlag,
    tx: Sender<WorkerMessage>,
) {
    thread::spawn(move || {
        let tx_emit = tx.clone();
        let ctx_emit = ctx.clone();
        // 这个闭包会被 detect 在**本线程**上调用（库后端也在本线程），
        // 所以不需要它是 Send
        let mut emit = move |ev: DetectEvent| {
            let _ = tx_emit.send(WorkerMessage::Detect(ev));
            ctx_emit.request_repaint();
        };

        let result = detector
            .detect(&files, &opts, &cancel, &mut emit)
            .map_err(|e| e.to_string());

        let _ = tx.send(WorkerMessage::DetectDone(result));
        ctx.request_repaint();
    });
}

/// 把 [`CutSink`] 实现成「往通道里发消息」。
struct ChannelCutSink {
    tx: Sender<WorkerMessage>,
    ctx: egui::Context,
}

impl CutSink for ChannelCutSink {
    fn line(&mut self, task_index: usize, line: &str) {
        let _ = self.tx.send(WorkerMessage::CutLine {
            index: task_index,
            line: line.to_string(),
        });
        self.ctx.request_repaint();
    }

    fn progress(&mut self, task_index: usize, fraction: f64) {
        let _ = self.tx.send(WorkerMessage::CutProgress {
            index: task_index,
            fraction: fraction as f32,
        });
        self.ctx.request_repaint();
    }

    fn finished(&mut self, task_index: usize, outcome: &CoreResult<CutOutcome>) {
        let result = match outcome {
            Ok(o) => Ok(CutInfo {
                output: o.output.clone(),
                kept_secs: o.kept_secs,
            }),
            Err(e) => Err(e.to_string()),
        };
        let _ = self.tx.send(WorkerMessage::CutFinished {
            index: task_index,
            result,
        });
        self.ctx.request_repaint();
    }
}

/// 后台批量切割。
pub fn spawn_cut(
    ctx: egui::Context,
    cutter: Cutter,
    tasks: Vec<EpisodeTask>,
    mode: CutMode,
    overwrite: bool,
    cancel: CancelFlag,
    tx: Sender<WorkerMessage>,
) {
    thread::spawn(move || {
        let mut sink = ChannelCutSink {
            tx: tx.clone(),
            ctx: ctx.clone(),
        };
        let summary = pipeline::cut_tasks(&cutter, &tasks, mode, overwrite, &cancel, &mut sink);
        let _ = tx.send(WorkerMessage::CutAllDone(summary));
        ctx.request_repaint();
    });
}

/// 抽帧预览文件名的进程内序号。
///
/// **为什么不能只靠时间戳**：`SystemTime::now()` 在本机只有约 1 毫秒的有效粒度，
/// 同一个下标背靠背起两次预览会算出同一个毫秒值，落到同一个路径上 —— 后一次
/// 覆写前一次，用户拖动时间点后看到的还是上一张画面。
///
/// **为什么也不能只靠序号**：临时目录是跨程序重启留存的，一个从 0 开始的序号
/// 会撞上上次运行留下的文件。两个一起用：序号保证进程内唯一，时间戳保证跨进程不撞。
static PREVIEW_SEQ: AtomicU64 = AtomicU64::new(0);

/// 后台抽一帧做预览缩略图。
pub fn spawn_preview(
    ctx: egui::Context,
    cutter: Cutter,
    video: PathBuf,
    at_secs: f64,
    index: usize,
    tx: Sender<WorkerMessage>,
) {
    thread::spawn(move || {
        // 抽帧结果落在系统临时目录，文件名 = 下标 + 毫秒时间戳 + 进程内序号，
        // 三者合起来才能保证「同一集反复预览不会互相覆盖看到旧图」
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = PREVIEW_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join("intro-outro-remover-preview");
        let png = dir.join(format!("frame-{index}-{stamp}-{seq}.png"));

        let msg = match cutter.extract_frame(&video, at_secs, &png) {
            Ok(()) => WorkerMessage::PreviewReady {
                index,
                png,
                at_secs,
            },
            Err(e) => WorkerMessage::PreviewFailed {
                index,
                error: e.to_string(),
            },
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    //! 用真实工具跑一遍后台线程管道。
    //!
    //! # 为什么测试写在二进制 crate 里面
    //!
    //! `crates/gui` 是二进制 crate，`tests/` 下的集成测试**没法** import 它的模块
    //! （二进制不产出可链接的 rlib）。所以只能写成 `#[cfg(test)]` 内联模块 ——
    //! 这也是本项目没给 gui 加 `tests/` 目录的原因。
    //!
    //! # 它补上了哪块空白
    //!
    //! core 的 e2e 覆盖的是**编排逻辑**，界面那一层要窗口测不了。中间这段
    //! 「后台线程 → 通道 → 消息」的管道原先一处测试都没有，而它恰好有三类
    //! 容易写错又不容易发现的问题：**消息漏发、下标串位、结果丢失**。
    //!
    //! 关键是 `egui::Context::default()` **不需要窗口**就能构造，而
    //! `request_repaint()` 在没人接的时候也只是记一个标志位 —— 所以这段完全
    //! 可以脱离窗口验证。
    //!
    //! 另外这里第一次覆盖了**抽帧预览**（`spawn_preview` → `Cutter::extract_frame`）：
    //! core 的 e2e 从来没碰过它，而它是界面上「预览」按钮的全部实现。
    //!
    //! # 怎么跑
    //!
    //! 默认 `#[ignore]`，因为要真跑 ffmpeg 与 needle。环境变量与 core 的 e2e
    //! 共用一套（`IOR_E2E_CLIPS` / `IOR_E2E_DIR` / `IOR_FFMPEG_DIR` / `IOR_NEEDLE_DIR`），
    //! 素材生成与具体用法见 `crates/core/tests/e2e.rs` 的模块文档。
    //!
    //! ```text
    //! cargo test -p intro-outro-gui -- --ignored --nocapture
    //! ```

    use super::*;
    use intro_outro_core::detect::new_cancel_flag;
    use intro_outro_core::model::TaskKind;
    use std::sync::atomic::Ordering;

    /// 把接收端收干。
    ///
    /// 能这么写是因为所有 `spawn_*` 都会在发完最后一条消息后结束线程、
    /// 丢弃自己那份发送端；调用方再丢掉手里这份，通道就关闭了，`iter()` 自然
    /// 终止 —— 不需要超时兜底。反过来，万一哪个线程在发消息前就崩了，通道
    /// 同样会关闭，这里会安静地返回一个短列表，由下面的长度断言抓出来。
    fn drain(rx: Receiver<WorkerMessage>) -> Vec<WorkerMessage> {
        rx.iter().collect()
    }

    /// 用一条共享通道跑一组后台任务，把消息全部收回来。
    ///
    /// 形状刻意贴近界面：**一条通道、多个任务、一个接收端**。任务闭包拿到的
    /// `tx` 想克隆几份都行，接收端始终只有这一个 —— 这正是 `App` 的做法。
    fn run(tasks: impl FnOnce(Sender<WorkerMessage>)) -> Vec<WorkerMessage> {
        let (tx, rx) = message_channel();
        tasks(tx.clone());
        drop(tx);
        drain(rx)
    }

    /// 把 Git Bash 风格的 `/c/Users/...` 翻成 Windows 风格。
    ///
    /// 不翻的话 `PathBuf::from("/c/...")` 会被解释成「当前盘根目录下的 `c\...`」，
    /// 报错只说「路径不存在」。与 core 的 e2e 里那段同理。
    fn normalise_path(p: PathBuf) -> PathBuf {
        if !cfg!(windows) {
            return p;
        }
        let s = p.to_string_lossy();
        let b = s.as_bytes();
        if b.len() >= 3 && b[0] == b'/' && b[2] == b'/' && (b[1] as char).is_ascii_alphabetic() {
            return PathBuf::from(format!(
                "{}:{}",
                (b[1] as char).to_ascii_uppercase(),
                &s[2..]
            ));
        }
        p
    }

    fn require_env(var: &str) -> PathBuf {
        let raw = std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                panic!("这个测试需要环境变量 {var}，用法见 crates/core/tests/e2e.rs 的模块文档")
            });
        normalise_path(PathBuf::from(raw))
    }

    /// 三条环境变量一起取，顺便把输出目录清空（否则会撞上「文件已存在」）。
    struct Setup {
        ctx: egui::Context,
        clips: PathBuf,
        out_dir: PathBuf,
        ffmpeg_dir: PathBuf,
        needle_dir: PathBuf,
    }

    impl Setup {
        fn new() -> Self {
            let clips = require_env("IOR_E2E_CLIPS");
            let out_dir = require_env("IOR_E2E_DIR");
            std::fs::create_dir_all(&out_dir).expect("建不出输出目录");
            for entry in std::fs::read_dir(&out_dir).into_iter().flatten().flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
            Self {
                ctx: egui::Context::default(),
                clips,
                out_dir,
                ffmpeg_dir: require_env("IOR_FFMPEG_DIR"),
                needle_dir: require_env("IOR_NEEDLE_DIR"),
            }
        }

        fn videos(&self) -> Vec<PathBuf> {
            pipeline::scan_videos(std::slice::from_ref(&self.clips), false).expect("扫描失败")
        }

        fn ffmpeg_tools(&self) -> FfmpegTools {
            FfmpegTools::discover(Some(self.ffmpeg_dir.as_path())).expect("找不到 ffmpeg")
        }
    }

    /// 检查一集的消息里有没有「下标越界」和「进度值跑出 0~1」。
    ///
    /// 后者是最经典的单位错误：把 ffmpeg 的 0~100 直接当 0~1 用，
    /// 界面上的进度条会立刻满格然后一直卡着，但程序其它部分完全正常。
    fn assert_indices_and_progress_are_sane(msgs: &[WorkerMessage], episodes: usize) {
        for msg in msgs {
            match msg {
                WorkerMessage::CutLine { index, .. } => {
                    assert!(
                        *index < episodes,
                        "消息里的下标 {index} 超出了 {episodes} 集"
                    )
                }
                WorkerMessage::CutProgress { index, fraction } => {
                    assert!(
                        *index < episodes,
                        "消息里的下标 {index} 超出了 {episodes} 集"
                    );
                    assert!(
                        fraction.is_finite() && (0.0..=1.0).contains(fraction),
                        "第 {index} 集的进度值 {fraction} 不在 0~1 之间"
                    )
                }
                _ => {}
            }
        }
    }

    /// 走完「工具检查 → 探测 → 检测 → 切割」全程，验证每一段都通过通道
    /// 把结果和进度如实送回了界面。
    #[test]
    #[ignore = "要真跑 ffmpeg 与 needle，需显式 --ignored 并设置 IOR_* 环境变量"]
    fn worker_pipeline_relays_every_stage_over_the_channel() {
        let setup = Setup::new();
        let videos = setup.videos();
        assert!(
            videos.len() >= 3,
            "素材不足（只有 {} 个），先用 tools/make_test_clips.sh 造至少 3 集",
            videos.len()
        );

        // ---- 1. 工具检查 --------------------------------------------------
        let msgs = run(|tx| {
            spawn_check_tools(
                setup.ctx.clone(),
                Some(setup.ffmpeg_dir.clone()),
                Some(setup.needle_dir.clone()),
                tx,
            );
        });
        assert_eq!(
            msgs.len(),
            1,
            "工具检查应该只发一条消息，实得 {} 条",
            msgs.len()
        );
        let info = match &msgs[0] {
            WorkerMessage::ToolsChecked(r) => r.as_ref().as_ref().expect("工具检查失败").clone(),
            other => panic!("第一条消息应该是 ToolsChecked，实得 {other:?}"),
        };
        assert!(info.ffmpeg.is_file(), "报出来的 ffmpeg 路径不是文件");
        assert!(
            !info.ffmpeg_version.is_empty(),
            "ffmpeg 版本号为空，顶栏会显示成空的"
        );
        let needle = info
            .needle
            .clone()
            .expect("这个测试依赖 needle，没找到就没法继续");
        println!(
            "工具检查：ffmpeg {} / needle {:?}",
            info.ffmpeg_version, info.needle_version
        );

        // ---- 2. 探测 ------------------------------------------------------
        let tools = setup.ffmpeg_tools();
        let cancel = new_cancel_flag();
        let msgs = run(|tx| {
            spawn_probe(
                setup.ctx.clone(),
                tools.clone(),
                videos.clone(),
                cancel.clone(),
                tx,
            );
        });
        let progress: Vec<(usize, usize)> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMessage::ProbeProgress { current, total, .. } => Some((*current, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(progress.len(), videos.len(), "探测的进度回调次数应等于集数");
        assert!(
            progress.iter().all(|(_, total)| *total == videos.len()),
            "进度里的总数不对"
        );
        assert_eq!(
            progress.last().map(|(c, _)| *c),
            Some(videos.len()),
            "最后一次进度应该走到总数"
        );
        let files = match msgs.last().expect("没有收到任何消息") {
            WorkerMessage::ProbeDone(r) => r.as_ref().expect("探测失败").clone(),
            other => panic!("最后一条消息应该是 ProbeDone，实得 {other:?}"),
        };
        assert_eq!(files.len(), videos.len(), "探测结果条数与集数不一致");
        for f in &files {
            assert!(f.duration_secs.is_some(), "{} 读不出时长", f.name());
            assert!(f.has_audio, "{} 没有音轨，检测会失败", f.name());
        }
        println!("探测完成 {} 集", files.len());

        // ---- 3. 检测 ------------------------------------------------------
        let detector = pick_detector(Some(&needle), false).expect("没有可用的检测后端");
        // 素材的片尾只给了 12 秒，低于 needle 默认阈值 20 秒，必须显式放低；
        // force_reanalyze 是为了不吃上一轮可能残留的缓存。
        let opts = DetectOptions {
            min_ending_duration_secs: 5,
            force_reanalyze: true,
            ..DetectOptions::default()
        };
        let msgs = run(|tx| {
            spawn_detect(
                setup.ctx.clone(),
                detector,
                videos.clone(),
                opts,
                cancel.clone(),
                tx,
            );
        });
        let done = msgs
            .iter()
            .filter(|m| matches!(m, WorkerMessage::Detect(DetectEvent::EpisodeDone { .. })))
            .count();
        assert_eq!(done, videos.len(), "逐集完成事件的数量应该等于集数");
        let failed = msgs
            .iter()
            .filter(|m| matches!(m, WorkerMessage::Detect(DetectEvent::EpisodeFailed { .. })))
            .count();
        assert_eq!(failed, 0, "有 {failed} 集分析失败");
        let detections = match msgs.last().expect("没有收到任何消息") {
            WorkerMessage::DetectDone(r) => r.as_ref().expect("检测失败").clone(),
            other => panic!("最后一条消息应该是 DetectDone，实得 {other:?}"),
        };
        assert_eq!(detections.len(), videos.len(), "检测结果条数与集数不一致");

        // 跨集对照 —— 真正能抓「结果串集」的断言：四集共用同一段片头，
        // 检出的片头起点必须互相接近。结果错位到别的文件上时这里立刻炸。
        let openings: Vec<f64> = detections
            .iter()
            .map(|d| {
                d.opening
                    .expect("这一集没检测到片头（检测结果可能串位了）")
                    .start
            })
            .collect();
        let first = openings[0];
        for (i, o) in openings.iter().enumerate() {
            assert!(
                (o - first).abs() < 2.0,
                "第 {} 集的片头起点 {o:.3}s 与第 1 集的 {first:.3}s 差太多 —— 检测结果可能串集了",
                i + 1
            );
        }
        assert!(
            detections.iter().all(|d| d.ending.is_some()),
            "有集没检测到片尾"
        );
        println!("检测完成：片头起点全部在 {first:.3}s 附近");

        // ---- 4. 切割 ------------------------------------------------------
        let tasks = pipeline::build_tasks(&files, &detections, &setup.out_dir, "_trimmed");
        assert_eq!(tasks.len(), files.len());
        assert!(
            tasks.iter().all(|t| t.kind == TaskKind::Ready),
            "素材应该全部可切，实得状态：{:?}",
            tasks.iter().map(|t| t.kind).collect::<Vec<_>>()
        );

        let cutter = Cutter::new(info.ffmpeg.clone());
        let msgs = run(|tx| {
            spawn_cut(
                setup.ctx.clone(),
                cutter.clone(),
                tasks.clone(),
                CutMode::Fast,
                true,
                cancel.clone(),
                tx,
            );
        });

        assert_indices_and_progress_are_sane(&msgs, tasks.len());

        let lines = msgs
            .iter()
            .filter(|m| matches!(m, WorkerMessage::CutLine { .. }))
            .count();
        assert!(
            lines > 0,
            "一条 ffmpeg 输出都没转发过来，界面上那个折叠的日志框会是空的"
        );

        let mut finished_indices: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMessage::CutFinished { index, result } => {
                    let info = result.as_ref().unwrap_or_else(|e| {
                        panic!("第 {index} 集切割失败：{e}");
                    });
                    assert!(info.output.is_file(), "第 {index} 集报成功但产物不存在");
                    assert!(info.kept_secs > 0.0, "第 {index} 集报成功但保留时长为 0");
                    Some(*index)
                }
                _ => None,
            })
            .collect();
        finished_indices.sort_unstable();
        assert_eq!(
            finished_indices,
            (0..tasks.len()).collect::<Vec<_>>(),
            "CutFinished 的下标必须一集不漏、且对得上任务顺序"
        );

        let progress_max = msgs
            .iter()
            .filter_map(|m| match m {
                WorkerMessage::CutProgress { fraction, .. } => Some(*fraction),
                _ => None,
            })
            .fold(0.0_f32, f32::max);
        assert!(
            progress_max >= 0.5,
            "最高进度只到 {progress_max}，进度根本没往前走"
        );

        match msgs.last().expect("没有收到任何消息") {
            WorkerMessage::CutAllDone(s) => {
                assert_eq!(s.succeeded, tasks.len(), "成功数不对：{s:?}");
                assert_eq!(s.failed, 0, "有切割失败：{s:?}");
                assert!(s.produced_secs > 0.0, "产出总时长为 0：{s:?}");
                println!(
                    "切割汇总：成功 {}，失败 {}，跳过 {}，产出 {:.1}s",
                    s.succeeded, s.failed, s.skipped, s.produced_secs
                );
            }
            other => panic!("最后一条消息应该是 CutAllDone，实得 {other:?}"),
        }

        println!("=== 后台管道全程通过 ===");
    }

    /// 抽帧预览：产物必须是真 PNG，而且**两次预览不能写到同一个文件上**。
    ///
    /// 文件名带时间戳就是为了「同一集反复预览时不会看到旧图」。这里特意
    /// **先把两个线程都起起来、再一起收**：如果中间隔着一次收干，两次取时间戳
    /// 之间至少隔了一次 ffmpeg 的耗时（几百毫秒），断言就永远碰不到「同一个
    /// 时间戳」的情况，等于白写。背靠背起线程才能真正压到时间戳粒度上。
    ///
    /// 这个测试是**真抓到过 bug 的**：改强之前它两次取同一样本、中间隔着收干，
    /// 属于假通过；改成背靠背后连跑三次三次全红（`as_millis()` 只有毫秒粒度，
    /// 同一下标的两次预览撞在同一个文件名上）。
    ///
    /// # 为什么不断言「两张图内容不一样」
    ///
    /// `tools/make_test_clips.sh` 造的画面是**整集一种纯色**（`color=c=...`），
    /// 同一集任意两个时间点抽出来的帧逐字节相同 —— 实测过：3.5s 与 40s 的原始
    /// 像素 md5 都是 `0985606f…`。所以「内容必须不同」这类断言在这套素材上**根本
    /// 不成立**，写上去只会得到一条恒假的断言。想连内容一起验，得先让素材带上
    /// 画面运动（比如把纯色换成 `testsrc`）。
    #[test]
    #[ignore = "要真跑 ffmpeg，需显式 --ignored 并设置 IOR_* 环境变量"]
    fn preview_writes_a_real_png_and_never_reuses_a_filename() {
        let setup = Setup::new();
        let videos = setup.videos();
        let video = videos.first().expect("没有素材").clone();
        let cutter = Cutter::new(setup.ffmpeg_tools().ffmpeg);

        // 同一集、两个不同时间点，背靠背起两个预览线程 —— 走的是一条共享通道，
        // 形状与界面上「点完第 1 集马上点第 2 集」完全一致
        let msgs = run(|tx| {
            spawn_preview(
                setup.ctx.clone(),
                cutter.clone(),
                video.clone(),
                3.5,
                0,
                tx.clone(),
            );
            spawn_preview(
                setup.ctx.clone(),
                cutter.clone(),
                video.clone(),
                40.0,
                0,
                tx,
            );
        });

        assert_eq!(
            msgs.len(),
            2,
            "两次预览应该各回一条消息，实得 {} 条 —— 缺的那条说明消息被丢在共享通道之外了",
            msgs.len()
        );

        let mut paths = Vec::new();
        for (round, (msg, want_secs)) in msgs.iter().zip([3.5_f64, 40.0]).enumerate() {
            match msg {
                WorkerMessage::PreviewReady {
                    index,
                    png,
                    at_secs,
                } => {
                    assert_eq!(*index, 0, "回传的下标必须与请求的一致");
                    assert!(
                        (*at_secs - want_secs).abs() < 1e-9,
                        "回传的时间点被改过了：第 {round} 次期望 {want_secs}，实得 {at_secs}"
                    );
                    let bytes = std::fs::read(png).expect("预览图没写出来");
                    assert!(
                        bytes.len() > 8,
                        "第 {round} 次预览的产物只有 {} 字节",
                        bytes.len()
                    );
                    assert_eq!(
                        &bytes[..8],
                        b"\x89PNG\r\n\x1a\n",
                        "产物不是 PNG（扩展名对不上内容）"
                    );
                    paths.push(png.clone());
                }
                other => panic!("期望 PreviewReady，实得 {other:?}"),
            }
        }
        assert_ne!(
            paths[0], paths[1],
            "两次预览写到了同一个文件上 —— 用户会看到上一张的旧图"
        );
        // 两张图得同时存在：撞名的症状之一就是前一张被后一张覆盖掉
        for (round, p) in paths.iter().enumerate() {
            assert!(p.is_file(), "第 {round} 张预览图不在磁盘上了：{p:?}");
        }
        println!("两次预览分别写到：{:?}", paths);

        // 收尾：预览图落在系统临时目录里，别留垃圾
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }

    /// 预览与批量任务同时跑时，**两条线各自的消息都不能丢**。
    ///
    /// 这条是冲着界面那段「所有任务共用一个 `Option<Receiver>` 槽」的历史缺陷来的：
    /// 起新任务会把上一条通道的接收端丢掉，被丢的那条线里 `send` 静默失败，
    /// 界面上对应的控件就永远等不到人来回位（典型症状：某一集的「预览」按钮
    /// 一直转圈、从此永久禁用）。
    ///
    /// 现在这条契约在**编译期**就成立：`spawn_*` 不再返回 `Receiver`，只收一个
    /// `Sender` 克隆，界面根本拿不到第二条通道。运行时这条测试再补一层：一边跑
    /// 探测、一边抽帧，两种消息都必须一条不少地回来。
    #[test]
    #[ignore = "要真跑 ffmpeg，需显式 --ignored 并设置 IOR_* 环境变量"]
    fn concurrent_preview_and_probe_both_deliver() {
        let setup = Setup::new();
        let videos = setup.videos();
        let video = videos.first().expect("没有素材").clone();
        let cutter = Cutter::new(setup.ffmpeg_tools().ffmpeg);
        let cancel = new_cancel_flag();

        let episodes = videos.len();
        let msgs = run(|tx| {
            spawn_probe(
                setup.ctx.clone(),
                setup.ffmpeg_tools(),
                videos.clone(),
                cancel.clone(),
                tx.clone(),
            );
            spawn_preview(setup.ctx.clone(), cutter.clone(), video.clone(), 3.5, 0, tx);
        });

        let previews = msgs
            .iter()
            .filter(|m| matches!(m, WorkerMessage::PreviewReady { .. }))
            .count();
        assert_eq!(
            previews, 1,
            "抽帧那条线被批量任务挤掉了 —— 缺的这条消息会让对应那一集的预览按钮永远转圈"
        );

        let probed = msgs
            .iter()
            .filter(|m| matches!(m, WorkerMessage::ProbeProgress { .. }))
            .count();
        assert_eq!(probed, episodes, "探测的进度回调次数应等于集数");
        assert!(
            matches!(msgs.last(), Some(WorkerMessage::ProbeDone(Ok(_)))),
            "最后一条消息应该是 ProbeDone(Ok)，实得 {:?}",
            msgs.last()
        );
        println!("并发下两类消息都到齐：预览 {previews} 条，探测进度 {probed} 条");

        // 收尾：把预览图删掉
        for m in &msgs {
            if let WorkerMessage::PreviewReady { png, .. } = m {
                let _ = std::fs::remove_file(png);
            }
        }
    }

    /// 提前置位的取消标志必须变成一条 `Err` 消息，而不是让线程悄悄结束。
    ///
    /// 悄悄结束是最糟的：界面上的「开始检测」按钮会一直转下去，用户只能重启程序。
    #[test]
    #[ignore = "要真跑 ffmpeg，需显式 --ignored 并设置 IOR_* 环境变量"]
    fn a_pre_set_cancel_flag_surfaces_as_an_error_message() {
        let setup = Setup::new();
        let videos = setup.videos();

        let cancelled = new_cancel_flag();
        cancelled.store(true, Ordering::Relaxed);

        let msgs = run(|tx| {
            spawn_probe(
                setup.ctx.clone(),
                setup.ffmpeg_tools(),
                videos,
                cancelled,
                tx,
            );
        });
        match msgs.last() {
            Some(WorkerMessage::ProbeDone(Err(e))) => {
                assert!(!e.is_empty(), "错误说明是空的，界面上会显示成一个空提示");
                println!("取消后的错误消息：{e}");
            }
            other => panic!("取消后应该收到 ProbeDone(Err)，实得 {other:?}"),
        }
    }
}
