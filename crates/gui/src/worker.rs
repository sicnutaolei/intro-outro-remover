//! 后台线程与界面之间的消息通道。
//!
//! # 线程模型
//!
//! 所有耗时工作（探测、检测、切割、抽帧）都跑在独立线程里，通过
//! `std::sync::mpsc` 把消息发回 UI 线程。UI 线程每帧 `try_recv` 把消息
//! 全部取干净再重绘。
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

/// 检查 ffmpeg / ffprobe / needle 是否可用，并取回版本号。
///
/// 放在后台线程里跑：这三个检查都要 spawn 进程，在启动路径上同步跑会让窗口
/// 出现之前卡顿一下，用户会以为程序启动慢。
pub fn spawn_check_tools(
    ctx: egui::Context,
    ffmpeg_dir: Option<PathBuf>,
    needle_dir: Option<PathBuf>,
) -> Receiver<WorkerMessage> {
    let (tx, rx) = channel();
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
    rx
}

/// 后台探测一批视频的媒体信息。
pub fn spawn_probe(
    ctx: egui::Context,
    tools: FfmpegTools,
    paths: Vec<PathBuf>,
    cancel: CancelFlag,
) -> Receiver<WorkerMessage> {
    let (tx, rx) = channel();
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
    rx
}

/// 后台跑检测。
pub fn spawn_detect(
    ctx: egui::Context,
    detector: DetectorChoice,
    files: Vec<PathBuf>,
    opts: DetectOptions,
    cancel: CancelFlag,
) -> Receiver<WorkerMessage> {
    let (tx, rx) = channel();

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

    rx
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
) -> Receiver<WorkerMessage> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut sink = ChannelCutSink {
            tx: tx.clone(),
            ctx: ctx.clone(),
        };
        let summary = pipeline::cut_tasks(&cutter, &tasks, mode, overwrite, &cancel, &mut sink);
        let _ = tx.send(WorkerMessage::CutAllDone(summary));
        ctx.request_repaint();
    });
    rx
}

/// 后台抽一帧做预览缩略图。
pub fn spawn_preview(
    ctx: egui::Context,
    cutter: Cutter,
    video: PathBuf,
    at_secs: f64,
    index: usize,
) -> Receiver<WorkerMessage> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        // 抽帧结果落在系统临时目录，文件名里带上毫秒时间戳，
        // 同一集反复预览时不会互相覆盖导致看到旧图
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join("intro-outro-remover-preview");
        let png = dir.join(format!("frame-{index}-{nanos}.png"));

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
    rx
}
