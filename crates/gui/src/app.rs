//! `eframe::App` 实现：界面布局、状态机、与后台线程的消息泵。
//!
//! # 界面结构
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ 工具栏：工具状态 · 序号 · 主题                                │
//! ├──────────────────────────────────┬───────────────────────────┤
//! │ 主区                              │ 预览面板                   │
//! │  ① 已导入文件（表格，时间戳可改）  │  当前选中集在指定时刻的     │
//! │  ② 检测参数                       │  画面，用来确认切点对不对   │
//! │  ③ 输出设置 + 开始切割             │                            │
//! ├──────────────────────────────────┴───────────────────────────┤
//! │ 日志（可折叠、可调整高度）                                     │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! # 一个刻意的设计：表格数据用 `mem::take` 换出来渲染
//!
//! 渲染表格需要一个闭包同时可变地访问 `rows` 和只读地访问其它字段。直接在
//! 闭包里借用 `self` 会被借用检查器拦下来（闭包捕获的是 `&mut self`，
//! 而 `ui` 又已经借走了它）。所以先把 `rows` 整体 `std::mem::take` 出来，
//! 渲染完再放回去 —— 零拷贝，只是搬一个 `Vec` 的头部。
//!
//! 另外，表格里发生的「用户点了某行的预览」这类动作不立即执行，而是压进
//! `actions` 向量，等表格渲染完再统一处理。否则在遍历过程中修改后台任务状态
//! 会引入一堆难以推理的交互。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use eframe::egui;
use egui_file_dialog::FileDialog;

use intro_outro_core::cut::{CutMode, Cutter};
use intro_outro_core::detect::{new_cancel_flag, CancelFlag, DetectEvent, DetectOptions};
use intro_outro_core::model::{
    format_timestamp, parse_timestamp, Detection, EpisodeFile, EpisodeTask, Segment,
};
use intro_outro_core::pipeline::{self, DEFAULT_OUTPUT_SUFFIX};
use intro_outro_core::probe::FfmpegTools;

use crate::fonts;
use crate::worker::{self, CutInfo, DetectorChoice, ToolInfo, WorkerMessage};

/// 日志最多留多少行。再多既没人看，也会让滚动区域变卡。
const MAX_LOG_LINES: usize = 1000;

/// 单集的运行时状态。
#[derive(Debug, Clone, PartialEq)]
enum RowStatus {
    /// 还没动过
    Idle,
    /// 排队等切割
    Queued,
    /// 正在切
    Cutting,
    /// 切好了
    Done { removed_secs: f64 },
    /// 切失败了
    Failed(String),
    /// 因为不需要切而跳过
    Skipped(String),
}

impl RowStatus {
    fn label(&self) -> String {
        match self {
            RowStatus::Idle => "—".to_string(),
            RowStatus::Queued => "排队中".to_string(),
            RowStatus::Cutting => "切割中".to_string(),
            RowStatus::Done { removed_secs } => format!("完成（去掉 {removed_secs:.1}s）"),
            RowStatus::Failed(e) => format!("失败：{e}"),
            RowStatus::Skipped(r) => format!("跳过：{r}"),
        }
    }

    fn color(&self, dark: bool) -> egui::Color32 {
        match self {
            RowStatus::Idle | RowStatus::Queued => {
                if dark {
                    egui::Color32::from_gray(150)
                } else {
                    egui::Color32::from_gray(110)
                }
            }
            RowStatus::Cutting => {
                if dark {
                    egui::Color32::from_rgb(120, 180, 255)
                } else {
                    egui::Color32::from_rgb(30, 90, 190)
                }
            }
            RowStatus::Done { .. } => {
                if dark {
                    egui::Color32::from_rgb(110, 210, 130)
                } else {
                    egui::Color32::from_rgb(20, 120, 50)
                }
            }
            RowStatus::Failed(_) => {
                if dark {
                    egui::Color32::from_rgb(255, 130, 130)
                } else {
                    egui::Color32::from_rgb(185, 28, 28)
                }
            }
            RowStatus::Skipped(_) => {
                if dark {
                    egui::Color32::from_rgb(230, 190, 110)
                } else {
                    egui::Color32::from_rgb(150, 100, 10)
                }
            }
        }
    }
}

/// 预览缩略图的状态。
#[derive(Default)]
struct PreviewState {
    /// 抽帧正在跑
    loading: bool,
    /// 已解码好的纹理
    texture: Option<egui::TextureHandle>,
    /// 这张图对应的时间点
    at_secs: Option<f64>,
    error: Option<String>,
}

/// 表格里一行。
struct Row {
    task: EpisodeTask,
    /// 四个可编辑时间戳的文本形式。
    ///
    /// 存文本而不是直接存 `f64`，是因为用户输入中途会出现「00:0」这种
    /// 语法上还不完整、但语义上不该报错的状态。存文本才能让输入过程自然。
    opening_start: String,
    opening_end: String,
    ending_start: String,
    ending_end: String,
    /// 时间戳解析失败的提示
    edit_error: Option<String>,

    status: RowStatus,
    /// 0.0 ~ 1.0，仅切割中有效
    progress: f32,
    /// 这一集的 ffmpeg 输出尾部
    log: VecDeque<String>,
    preview: PreviewState,
}

impl Row {
    fn new(task: EpisodeTask) -> Self {
        let mut row = Self {
            task,
            opening_start: String::new(),
            opening_end: String::new(),
            ending_start: String::new(),
            ending_end: String::new(),
            edit_error: None,
            status: RowStatus::Idle,
            progress: 0.0,
            log: VecDeque::new(),
            preview: PreviewState::default(),
        };
        row.reset_texts_from_detection();
        row
    }

    /// 把检测结果写回文本框（导入检测结果、或用户点「重置」时调）。
    fn reset_texts_from_detection(&mut self) {
        let (os, oe) = split_text(self.task.detection.opening);
        let (es, ee) = split_text(self.task.detection.ending);
        self.opening_start = os;
        self.opening_end = oe;
        self.ending_start = es;
        self.ending_end = ee;
        self.edit_error = None;
    }

    /// 把文本框内容解析回检测结果。
    ///
    /// 解析失败时**只提示、不改数据** —— 把用户的笔误静默当成 0 会切错片。
    fn apply_text_edits(&mut self) {
        let os = resolve_field(&self.opening_start);
        let oe = resolve_field(&self.opening_end);
        let es = resolve_field(&self.ending_start);
        let ee = resolve_field(&self.ending_end);

        let mut errors = Vec::new();
        for (label, r) in [
            ("片头起点", &os),
            ("片头终点", &oe),
            ("片尾起点", &es),
            ("片尾终点", &ee),
        ] {
            if let Err(e) = r {
                errors.push(format!("{label}{e}"));
            }
        }
        if !errors.is_empty() {
            self.edit_error = Some(errors.join("；"));
            return;
        }
        self.edit_error = None;

        self.task.detection = Detection {
            opening: join_segment(os.unwrap_or(None), oe.unwrap_or(None)),
            ending: join_segment(es.unwrap_or(None), ee.unwrap_or(None)),
        };
        // 检测结果变了，切割计划必须跟着重算，否则切出来还是旧时间戳
        pipeline::recompute_task(&mut self.task);
    }

    /// 该用哪一刻的画面做预览。
    ///
    /// 优先抽「片头刚结束」这一帧 —— 那是用户最需要确认的位置：切在这里，
    /// 正片开头有没有被误伤、画面是不是接得上。没有片头就抽片尾开始处。
    fn preview_time(&self) -> f64 {
        if let Some(o) = self.task.detection.opening {
            return o.end;
        }
        if let Some(e) = self.task.detection.ending {
            return e.start;
        }
        // 什么都没检测到，就抽 10% 处，总比空白强
        self.task.file.duration_secs.map(|d| d * 0.1).unwrap_or(0.0)
    }
}

/// 一条日志。
struct LogLine {
    level: LogLevel,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LogLevel {
    Info,
    Warn,
    Error,
}

/// 当前在忙什么。用来禁用按钮、决定进度条怎么画。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Busy {
    Idle,
    Probing,
    Detecting,
    Cutting,
}

/// 文件对话框里的动作，延迟到表格渲染完再执行。
enum RowAction {
    Preview(usize),
    ResetRow(usize),
}

pub struct App {
    /// egui 上下文。存一份是因为启动后台任务时要把它克隆给线程
    /// （后台线程靠它 `request_repaint` 把界面叫醒）。
    ctx: egui::Context,

    // ---- 外部工具 ----
    tools: Option<FfmpegTools>,
    cutter: Option<Cutter>,
    tool_info: Option<ToolInfo>,
    tool_error: Option<String>,
    /// 用户手动指定的工具目录（留空则查 PATH）
    ffmpeg_dir: String,
    needle_dir: String,

    // ---- 输入 ----
    rows: Vec<Row>,
    recursive_scan: bool,
    /// 预览面板当前展示哪一行
    preview_row: Option<usize>,

    // ---- 检测参数 ----
    /// 检测后端。`None` 表示还没确定（工具检查未完成，或 needle 没找到）。
    detector: Option<DetectorChoice>,
    /// 同时具备两种后端时是否优先用库直链（只有编译期开了 `needle-lib` 才有意义）
    prefer_library: bool,
    detect_opts: DetectOptions,

    // ---- 输出 ----
    out_dir: Option<PathBuf>,
    out_suffix: String,
    overwrite: bool,
    cut_mode: CutMode,

    // ---- 运行时 ----
    busy: Busy,
    rx: Option<Receiver<WorkerMessage>>,
    cancel: CancelFlag,
    stage: String,
    /// （当前, 总数）
    counter: (usize, usize),
    progress: f32,
    logs: VecDeque<LogLine>,

    // ---- 杂项 ----
    theme: egui::ThemePreference,
    font_loaded: Option<String>,
    /// 找过但没读成的字体路径。仅在中文字体没加载成功时非空。
    font_attempted: Vec<String>,
    dialog_add: FileDialog,
    dialog_out: FileDialog,
    status_line: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font_setup = fonts::install_cjk_fonts(&cc.egui_ctx);
        if font_setup.loaded_from.is_none() {
            tracing::warn!("未找到中文字体，界面可能显示为方块");
        }

        // 两个对话框各带一个唯一 id：混用一个实例的话，
        // 「选文件」和「选目录」的结果会互相污染
        let dialog_add = FileDialog::new()
            .id(egui::Id::new("dialog-add"))
            .title("选择视频文件或文件夹");
        let dialog_out = FileDialog::new()
            .id(egui::Id::new("dialog-out"))
            .title("选择输出目录");

        let ctx = cc.egui_ctx.clone();
        // 工具检查要 spawn 进程，放后台跑，别卡住窗口首帧
        let rx = worker::spawn_check_tools(ctx.clone(), None, None);

        Self {
            ctx,
            tools: None,
            cutter: None,
            tool_info: None,
            tool_error: None,
            ffmpeg_dir: String::new(),
            needle_dir: String::new(),
            rows: Vec::new(),
            recursive_scan: false,
            preview_row: None,
            // 具体用哪个后端要等工具检查结果出来才知道，先留空
            detector: None,
            prefer_library: false,
            detect_opts: DetectOptions::default(),
            out_dir: None,
            out_suffix: DEFAULT_OUTPUT_SUFFIX.to_string(),
            overwrite: false,
            cut_mode: CutMode::Fast,
            busy: Busy::Idle,
            rx: Some(rx),
            cancel: new_cancel_flag(),
            stage: String::new(),
            counter: (0, 0),
            progress: 0.0,
            logs: VecDeque::new(),
            theme: egui::ThemePreference::Light,
            font_loaded: font_setup.loaded_from,
            font_attempted: font_setup.attempted,
            dialog_add,
            dialog_out,
            status_line: "正在检查 ffmpeg / needle…".to_string(),
        }
    }

    // -----------------------------------------------------------------
    // 消息泵
    // -----------------------------------------------------------------

    /// 把后台线程发来的消息全部处理掉。
    ///
    /// 用 `try_recv` 循环而不是只取一条：一帧里可能堆了好几条，逐帧只取一条
    /// 会让日志和进度明显滞后。
    fn drain_messages(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        let mut received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            received.push(msg);
        }
        for msg in received {
            self.handle_message(ctx, msg);
        }
    }

    fn handle_message(&mut self, ctx: &egui::Context, msg: WorkerMessage) {
        match msg {
            WorkerMessage::ToolsChecked(result) => match *result {
                Ok(info) => {
                    self.status_line = format!(
                        "ffmpeg：{}　needle：{}",
                        short_version(&info.ffmpeg_version),
                        info.needle_version
                            .clone()
                            .unwrap_or_else(|| "未找到".to_string())
                    );
                    self.ffmpeg_dir = info
                        .ffmpeg
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();

                    // 用 non_empty 而不是直接 Path::new("")：空字符串会把
                    // 「相对当前目录的 ffmpeg.exe」当成候选，可能误命中别的东西
                    let ff_dir = non_empty(&self.ffmpeg_dir);
                    self.tools = FfmpegTools::discover(ff_dir.as_deref()).ok();
                    if let Some(t) = &self.tools {
                        self.cutter = Some(Cutter::new(t.ffmpeg.clone()));
                    }

                    if let Some(np) = &info.needle {
                        self.needle_dir = np
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                    }
                    self.detector =
                        worker::pick_detector(info.needle.as_ref(), self.prefer_library);

                    self.log(LogLevel::Info, format!("ffmpeg：{}", info.ffmpeg.display()));
                    if let Some(v) = &info.needle_version {
                        self.log(LogLevel::Info, format!("needle：{v}"));
                    }
                    match &self.detector {
                        Some(d) => self.log(LogLevel::Info, format!("检测后端：{}", d.label())),
                        None => self.log(
                            LogLevel::Warn,
                            "没找到 needle，自动检测不可用。你仍然可以手动填片头片尾时间戳后\
                             直接切割；或者把 needle 可执行文件放进 PATH，再点「重新检查工具」。\
                             （needle 官方 release 有单文件 Windows 版，下载解压即用）",
                        ),
                    }
                    self.tool_error = None;
                    self.tool_info = Some(info);
                }
                Err(e) => {
                    self.tool_error = Some(e.clone());
                    self.status_line = "ffmpeg 不可用".to_string();
                    self.log(LogLevel::Error, e);
                }
            },

            WorkerMessage::ProbeProgress {
                current,
                total,
                name,
            } => {
                self.counter = (current, total);
                self.progress = if total == 0 {
                    0.0
                } else {
                    current as f32 / total as f32
                };
                self.stage = format!("探测媒体信息：{name}");
            }
            WorkerMessage::ProbeDone(result) => {
                self.busy = Busy::Idle;
                match result {
                    Ok(files) => {
                        let count = files.len();
                        self.build_rows(files);
                        self.log(LogLevel::Info, format!("导入完成，共 {count} 个视频文件"));
                        if count < 2 {
                            self.log(
                                LogLevel::Warn,
                                "检测需要至少 2 集才能做跨集比对。只有 1 集时请手动填时间戳。"
                                    .to_string(),
                            );
                        }
                        self.status_line = format!("已导入 {count} 个文件");
                    }
                    Err(e) => {
                        self.log(LogLevel::Error, format!("探测失败：{e}"));
                        self.status_line = "探测失败".to_string();
                    }
                }
                self.stage.clear();
                self.progress = 0.0;
            }

            WorkerMessage::Detect(ev) => self.handle_detect_event(ev),
            WorkerMessage::DetectDone(result) => {
                self.busy = Busy::Idle;
                self.stage.clear();
                match result {
                    Ok(detections) => {
                        let mut found = 0usize;
                        for (i, det) in detections.iter().enumerate() {
                            if let Some(row) = self.rows.get_mut(i) {
                                row.task.detection = det.clone();
                                row.reset_texts_from_detection();
                                pipeline::recompute_task(&mut row.task);
                                row.status = RowStatus::Idle;
                                if !det.is_empty() {
                                    found += 1;
                                }
                            }
                        }
                        self.log(
                            LogLevel::Info,
                            format!(
                                "检测完成：{} 集里 {found} 集找到了片头或片尾",
                                self.rows.len()
                            ),
                        );
                        // 检查一下各集之间片头时间是否差异过大 —— needle 是按
                        // 最长公共片段给的单一结果，若各集差异很大通常意味着
                        // 这批剧集里混进了别的剧（或某一集的音轨就是坏的）
                        self.warn_on_inconsistent_openings();
                        self.status_line = format!("检测完成，{found} 集有结果");
                    }
                    Err(e) => {
                        self.log(LogLevel::Error, format!("检测失败：{e}"));
                        self.status_line = "检测失败".to_string();
                    }
                }
            }

            WorkerMessage::CutLine { index, line } => {
                if let Some(row) = self.rows.get_mut(index) {
                    if row.log.len() >= 200 {
                        row.log.pop_front();
                    }
                    row.log.push_back(line.clone());
                }
                self.log(LogLevel::Info, line);
            }
            WorkerMessage::CutProgress { index, fraction } => {
                if let Some(row) = self.rows.get_mut(index) {
                    row.progress = fraction.clamp(0.0, 1.0);
                }
            }
            WorkerMessage::CutFinished { index, result } => {
                let (msg, level) = match &result {
                    Ok(CutInfo { output, kept_secs }) => (
                        format!(
                            "[{}] 完成，保留 {:.1}s -> {}",
                            index + 1,
                            kept_secs,
                            output
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ),
                        LogLevel::Info,
                    ),
                    Err(e) => (format!("[{}] 失败：{e}", index + 1), LogLevel::Error),
                };
                if let Some(row) = self.rows.get_mut(index) {
                    row.progress = 1.0;
                    // 先把 removed_secs 取出来再赋值 status：直接在赋值右侧读
                    // row.task 会跟左侧的 &mut row.status 撞借用
                    let removed = row.task.removed_secs().unwrap_or(0.0);
                    row.status = match result {
                        Ok(_) => RowStatus::Done {
                            removed_secs: removed,
                        },
                        Err(e) => RowStatus::Failed(e),
                    };
                }
                self.log(level, msg);
            }
            WorkerMessage::CutAllDone(summary) => {
                self.busy = Busy::Idle;
                self.stage.clear();
                self.progress = 1.0;
                self.status_line = format!(
                    "切割结束：成功 {}，失败 {}，跳过 {}，共产出 {:.1} 分钟",
                    summary.succeeded,
                    summary.failed,
                    summary.skipped,
                    summary.produced_secs / 60.0
                );
                let level = if summary.failed > 0 {
                    LogLevel::Warn
                } else {
                    LogLevel::Info
                };
                self.log(level, self.status_line.clone());

                if summary.succeeded > 0 {
                    if let Some(dir) = &self.out_dir {
                        let dir = dir.clone();
                        open_in_file_manager(&dir);
                    }
                }
            }

            WorkerMessage::PreviewReady {
                index,
                png,
                at_secs,
            } => {
                self.load_preview_texture(ctx, index, &png, at_secs);
            }
            WorkerMessage::PreviewFailed { index, error } => {
                if let Some(row) = self.rows.get_mut(index) {
                    row.preview.loading = false;
                    row.preview.error = Some(error.clone());
                }
                self.log(LogLevel::Warn, format!("抽帧预览失败：{error}"));
            }
        }
    }

    fn handle_detect_event(&mut self, ev: DetectEvent) {
        match ev {
            DetectEvent::Stage(s) => {
                self.stage = s.clone();
                self.log(LogLevel::Info, format!("== {s} =="));
            }
            DetectEvent::Log(l) => self.log(LogLevel::Info, l),
            DetectEvent::EpisodeDone { index, total, name } => {
                self.counter = (index, total);
                self.progress = index as f32 / total.max(1) as f32;
                self.log(LogLevel::Info, format!("分析完成 [{index}/{total}] {name}"));
            }
            DetectEvent::EpisodeFailed {
                index,
                total,
                name,
                error,
            } => {
                self.counter = (index, total);
                self.progress = index as f32 / total.max(1) as f32;
                self.log(
                    LogLevel::Warn,
                    format!("分析失败 [{index}/{total}] {name}：{error}"),
                );
            }
        }
    }

    /// 把 PNG 解码成 egui 纹理。
    ///
    /// 自己解码而不用 egui 的 `file://` 图片加载器，是因为 Windows 路径里
    /// 反斜杠、空格、中文都会出问题，手动解码一劳永逸。
    fn load_preview_texture(
        &mut self,
        ctx: &egui::Context,
        index: usize,
        png: &Path,
        at_secs: f64,
    ) {
        let decoded = image::open(png).map(|img| {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            (size, rgba.into_raw())
        });

        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.preview.loading = false;
        match decoded {
            Ok((size, pixels)) => {
                let color = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                row.preview.texture = Some(ctx.load_texture(
                    format!("preview-{index}"),
                    color,
                    egui::TextureOptions::LINEAR,
                ));
                row.preview.at_secs = Some(at_secs);
                row.preview.error = None;
            }
            Err(e) => {
                row.preview.error = Some(format!("图片解码失败：{e}"));
            }
        }
        // 临时 PNG 用完就删，别在系统临时目录里堆垃圾
        let _ = std::fs::remove_file(png);
    }

    /// 各集片头起点差异过大时提醒一句。
    ///
    /// needle 给的是「这一集在别集里能找到的最长公共片段」，如果各集差得离谱，
    /// 通常意味着这批文件里混进了别的剧、或者某一集没音轨 —— 直接切下去就毁了。
    fn warn_on_inconsistent_openings(&mut self) {
        let starts: Vec<f64> = self
            .rows
            .iter()
            .filter_map(|r| r.task.detection.opening.map(|o| o.start))
            .collect();
        if starts.len() < 2 {
            return;
        }
        let min = starts.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = starts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if max - min > 30.0 {
            self.log(
                LogLevel::Warn,
                format!(
                    "各集片头起点相差 {:.0} 秒（最早 {:.1}s，最晚 {:.1}s），\
                     差异偏大。请用右侧预览逐集确认切点，或检查这批文件是不是同一个剧集。",
                    max - min,
                    min,
                    max
                ),
            );
        }
    }

    fn log(&mut self, level: LogLevel, text: impl Into<String>) {
        let text = text.into();
        tracing::debug!(target: "ui", "{text}");
        if self.logs.len() >= MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(LogLine { level, text });
    }

    // -----------------------------------------------------------------
    // 任务启动
    // -----------------------------------------------------------------

    fn build_rows(&mut self, files: Vec<EpisodeFile>) {
        let out_dir = self
            .out_dir
            .clone()
            .or_else(|| {
                files
                    .first()
                    .and_then(|f| f.path.parent())
                    .map(|p| p.to_path_buf())
            })
            .unwrap_or_else(|| PathBuf::from("."));
        self.out_dir = Some(out_dir.clone());

        let detections: Vec<Detection> = files.iter().map(|_| Detection::default()).collect();
        let tasks = pipeline::build_tasks(&files, &detections, &out_dir, &self.out_suffix);
        self.rows = tasks.into_iter().map(Row::new).collect();
        self.preview_row = None;
    }

    /// 把新拖入 / 选中的路径合并进现有列表。
    fn add_inputs(&mut self, paths: Vec<PathBuf>) {
        if self.busy != Busy::Idle {
            self.log(LogLevel::Warn, "任务进行中，暂不接受新文件".to_string());
            return;
        }
        let scanned = match pipeline::scan_videos(&paths, self.recursive_scan) {
            Ok(v) => v,
            Err(e) => {
                self.log(LogLevel::Error, format!("扫描失败：{e}"));
                return;
            }
        };
        if scanned.is_empty() {
            self.log(
                LogLevel::Warn,
                format!(
                    "这些路径里没有找到视频文件（支持：{}）",
                    pipeline::VIDEO_EXTENSIONS.join(" / ")
                ),
            );
            return;
        }

        // 去重：同一个文件拖两次不该出现两行
        let existing: std::collections::HashSet<PathBuf> =
            self.rows.iter().map(|r| r.task.file.path.clone()).collect();
        let fresh: Vec<PathBuf> = scanned
            .into_iter()
            .filter(|p| !existing.contains(p))
            .collect();

        if fresh.is_empty() {
            self.log(LogLevel::Info, "这些文件已经在列表里了".to_string());
            return;
        }

        let Some(tools) = self.tools.clone() else {
            self.log(
                LogLevel::Error,
                "ffmpeg 不可用，无法读取视频信息。请先安装 ffmpeg 或在本页指定它的目录。"
                    .to_string(),
            );
            return;
        };

        self.log(
            LogLevel::Info,
            format!("开始探测 {} 个新文件的媒体信息…", fresh.len()),
        );
        self.busy = Busy::Probing;
        self.cancel = new_cancel_flag();
        self.rx = Some(worker::spawn_probe(
            self.ctx.clone(),
            tools,
            fresh,
            self.cancel.clone(),
        ));
    }

    /// 检测后端在界面上的显示名。
    fn detector_label(&self) -> &'static str {
        match &self.detector {
            Some(d) => d.label(),
            None => "未就绪",
        }
    }

    fn start_detect(&mut self) {
        if self.busy != Busy::Idle {
            return;
        }
        let files: Vec<PathBuf> = self.rows.iter().map(|r| r.task.file.path.clone()).collect();
        if files.len() < 2 {
            self.log(
                LogLevel::Warn,
                "检测需要至少 2 集。只有 1 集的时候请手动填写片头片尾时间戳，然后直接切割。"
                    .to_string(),
            );
            return;
        }
        let Some(detector) = self.detector.clone() else {
            self.log(
                LogLevel::Error,
                "没有可用的检测后端（没找到 needle）。请把 needle 装好并放进 PATH 后点\
                 「重新检查工具」；或者手动填写片头片尾时间戳，然后直接切割。"
                    .to_string(),
            );
            return;
        };
        self.log(LogLevel::Info, format!("检测后端：{}", detector.describe()));
        if !detector.has_fine_grained_progress() {
            self.log(
                LogLevel::Warn,
                "当前后端是库直链，无法回报逐集进度，也无法在阶段中途取消 —— 这是 \
                 needle-rs 库接口本身的限制（分析调用没有回调也没有取消入口）。"
                    .to_string(),
            );
        }

        self.busy = Busy::Detecting;
        self.cancel = new_cancel_flag();
        self.counter = (0, files.len());
        self.progress = 0.0;
        self.rx = Some(worker::spawn_detect(
            self.ctx.clone(),
            detector,
            files,
            self.detect_opts.clone(),
            self.cancel.clone(),
        ));
    }

    fn start_cut(&mut self) {
        if self.busy != Busy::Idle {
            return;
        }
        let Some(cutter) = self.cutter.clone() else {
            self.log(LogLevel::Error, "ffmpeg 不可用，无法切割".to_string());
            return;
        };
        let Some(out_dir) = self.out_dir.clone() else {
            self.log(LogLevel::Warn, "还没选择输出目录".to_string());
            return;
        };

        let actionable = self
            .rows
            .iter()
            .filter(|r| r.task.kind.is_actionable())
            .count();
        if actionable == 0 {
            self.log(
                LogLevel::Warn,
                "没有可切割的剧集。要么还没跑检测，要么所有集都没检测到片头片尾。".to_string(),
            );
            return;
        }

        // 重建一遍任务，把用户手改的时间戳和最新的输出目录 / 后缀带上
        let mut tasks: Vec<EpisodeTask> = Vec::with_capacity(self.rows.len());
        for row in &mut self.rows {
            row.task.output =
                pipeline::output_path_for(&row.task.file.path, &out_dir, &self.out_suffix);
            pipeline::recompute_task(&mut row.task);
            row.status = if row.task.kind.is_actionable() {
                RowStatus::Queued
            } else {
                RowStatus::Skipped(row.task.kind.label().to_string())
            };
            row.progress = 0.0;
            row.log.clear();
            tasks.push(row.task.clone());
        }

        self.busy = Busy::Cutting;
        self.cancel = new_cancel_flag();
        self.progress = 0.0;
        self.stage = "开始切割".to_string();
        self.log(
            LogLevel::Info,
            format!(
                "开始切割 {actionable} 集，输出到 {}（{}）",
                out_dir.display(),
                self.cut_mode.label()
            ),
        );
        self.rx = Some(worker::spawn_cut(
            self.ctx.clone(),
            cutter,
            tasks,
            self.cut_mode,
            self.overwrite,
            self.cancel.clone(),
        ));
    }

    fn request_preview(&mut self, index: usize) {
        let Some(cutter) = self.cutter.clone() else {
            self.log(LogLevel::Error, "ffmpeg 不可用，无法抽帧".to_string());
            return;
        };
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.preview.loading = true;
        row.preview.error = None;
        self.preview_row = Some(index);

        let video = row.task.file.path.clone();
        let at = row.preview_time();
        self.rx = Some(worker::spawn_preview(
            self.ctx.clone(),
            cutter,
            video,
            at,
            index,
        ));
    }
}

impl eframe::App for App {
    /// eframe 0.36 起 `App` 的入口是 `ui` 而不是 `update`：拿到的是一个
    /// 已经铺满窗口的裸 `Ui`（无背景无内外边距），面板挂在它上面。
    ///
    /// 顺手把 `Context` 克隆一份出来 —— 它是 `Arc` 包装，克隆很便宜，
    /// 但这样后面那些「只需要 ctx」的辅助函数就不必都改成接收 `ui`。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        ctx.set_theme(self.theme);
        self.drain_messages(&ctx);

        self.handle_dropped_files(&ctx);
        // 两个文件对话框每帧都要 update 才会真正渲染出来；
        // 只在点按钮时调是不够的 —— 那一点击之后就再没机会画了
        self.dialog_add.update(&ctx);
        self.dialog_out.update(&ctx);
        self.pump_file_dialogs();

        // 有后台任务在跑时持续重绘，让「已用时间」「进度条」这类东西动起来。
        // 单靠消息触发的 request_repaint 在长时间无输出的阶段（比如 needle
        // 的比对阶段）会让界面看起来冻住。
        if self.busy != Busy::Idle {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        // 顺序有讲究：先四周的面板，最后才是 CentralPanel —— 中央面板
        // 拿走的是「剩下」的空间，提前放会让四周面板没地方长。
        self.top_bar(ui);
        self.log_panel(ui);
        self.preview_panel(ui);
        self.main_panel(ui);
        self.drop_overlay(&ctx);
    }
}

// ---------------------------------------------------------------------
// 界面各区块
// ---------------------------------------------------------------------

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading("剧集片头片尾批量去除");
                ui.separator();

                // 工具状态灯
                if let Some(err) = &self.tool_error {
                    ui.colored_label(egui::Color32::from_rgb(185, 28, 28), "ffmpeg 未就绪")
                        .on_hover_text(tool_tooltip(self.tool_info.as_ref()));
                    ui.label(
                        egui::RichText::new(err.as_str())
                            .small()
                            .color(egui::Color32::from_gray(120)),
                    );
                } else {
                    ui.colored_label(egui::Color32::from_rgb(20, 120, 50), "● ffmpeg 就绪")
                        .on_hover_text(tool_tooltip(self.tool_info.as_ref()));
                    ui.label(
                        egui::RichText::new(self.status_line.as_str())
                            .small()
                            .color(egui::Color32::from_gray(120)),
                    );
                }

                ui.separator();
                ui.label("主题：");
                ui.selectable_value(&mut self.theme, egui::ThemePreference::Light, "浅色");
                ui.selectable_value(&mut self.theme, egui::ThemePreference::Dark, "深色");
                ui.selectable_value(&mut self.theme, egui::ThemePreference::System, "跟随系统");
            });

            if self.font_loaded.is_none() {
                // 把「去哪儿找过」摊开给用户看 —— 只说「未找到字体」他无从下手
                let tried = if self.font_attempted.is_empty() {
                    "（没有可用的候选路径）".to_string()
                } else {
                    format!(
                        "已尝试 {} 处：\n{}",
                        self.font_attempted.len(),
                        self.font_attempted.join("\n")
                    )
                };
                ui.colored_label(
                    egui::Color32::from_rgb(150, 100, 10),
                    "⚠ 没找到系统中文字体，界面上的中文可能显示为方块",
                )
                .on_hover_text(tried);
            }

            ui.add_space(4.0);
        });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        // 0.36 把 SidePanel / TopBottomPanel 合并成了 Panel，尺寸相关的方法也
        // 从 default_width/default_height 统一成 default_size / min_size
        // —— 轴向由 left/right/top/bottom 决定，不再需要区分宽高。
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(170.0)
            .min_size(60.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("运行日志");
                    ui.label(
                        egui::RichText::new(format!("（{} 行）", self.logs.len()))
                            .small()
                            .color(egui::Color32::from_gray(120)),
                    );
                    ui.separator();
                    if ui.button("清空日志").clicked() {
                        self.logs.clear();
                    }
                    if ui.button("复制全部").clicked() {
                        let all: String = self
                            .logs
                            .iter()
                            .map(|l| l.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.ctx().copy_text(all);
                    }
                    ui.separator();
                    if self.busy != Busy::Idle
                        && ui
                            .button(
                                egui::RichText::new("■ 取消当前任务")
                                    .color(egui::Color32::from_rgb(185, 28, 28)),
                            )
                            .clicked()
                    {
                        self.cancel
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        self.log(
                            LogLevel::Warn,
                            "已请求取消，正在等待当前步骤停下…".to_string(),
                        );
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.logs {
                            let color = match line.level {
                                LogLevel::Info => ui.visuals().text_color(),
                                LogLevel::Warn => egui::Color32::from_rgb(150, 100, 10),
                                LogLevel::Error => egui::Color32::from_rgb(185, 28, 28),
                            };
                            ui.label(
                                egui::RichText::new(line.text.as_str())
                                    .color(color)
                                    .monospace(),
                            );
                        }
                    });
            });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::right("preview")
            .resizable(true)
            .default_size(380.0)
            .min_size(220.0)
            .show(ui, |ui| {
                ui.strong("切点预览");
                ui.label(
                    egui::RichText::new("抽的是「片头刚结束」那一帧 —— 确认正片开头没被误伤")
                        .small()
                        .color(egui::Color32::from_gray(120)),
                );
                ui.separator();

                let Some(index) = self.preview_row else {
                    ui.label(
                        egui::RichText::new("在左侧表格里点某一行的「预览」按钮")
                            .color(egui::Color32::from_gray(120)),
                    );
                    return;
                };
                let Some(row) = self.rows.get(index) else {
                    return;
                };

                ui.label(format!("{} · 第 {} 行", row.task.file.name(), index + 1));

                if row.preview.loading {
                    ui.add(egui::Spinner::new());
                    ui.label("正在抽帧…");
                    return;
                }
                if let Some(err) = &row.preview.error {
                    ui.colored_label(egui::Color32::from_rgb(185, 28, 28), err);
                    return;
                }
                let Some(texture) = &row.preview.texture else {
                    ui.label("还没有预览图");
                    return;
                };

                if let Some(at) = row.preview.at_secs {
                    ui.label(format!("时刻：{}", format_timestamp(at)));
                }
                let avail = ui.available_width();
                let size = texture.size_vec2();
                // 等比缩放到面板宽度内，别把面板撑变形
                let scale = (avail / size.x).min(1.0);
                let shown = size * scale;
                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    texture.id(),
                    shown,
                )));

                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "提示：如果这一帧已经开始播片头的歌，说明片头终点标早了；\
                         如果是黑屏或上一集的画面，说明标晚了。用左侧的文本框微调后重新预览。",
                    )
                    .small()
                    .color(egui::Color32::from_gray(120)),
                );
            });
    }

    fn main_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.import_section(ui);
                    ui.add_space(8.0);
                    ui.separator();
                    self.table_section(ui);
                    ui.add_space(8.0);
                    ui.separator();
                    self.detect_section(ui);
                    ui.add_space(8.0);
                    ui.separator();
                    self.output_section(ui);
                    ui.add_space(12.0);
                });
        });
    }

    fn import_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("① 导入");
            let can_add = self.busy == Busy::Idle;

            if ui
                .add_enabled(can_add, egui::Button::new("添加文件…"))
                .clicked()
            {
                self.dialog_add.pick_multiple();
            }
            if ui
                .add_enabled(can_add, egui::Button::new("添加文件夹…"))
                .clicked()
            {
                self.dialog_add.pick_directory();
            }
            ui.checkbox(&mut self.recursive_scan, "包含子文件夹");
            ui.separator();
            if ui
                .add_enabled(can_add, egui::Button::new("清空列表"))
                .clicked()
            {
                self.rows.clear();
                self.preview_row = None;
                self.status_line = "列表已清空".to_string();
            }
            ui.separator();
            ui.label(
                egui::RichText::new("也可以直接把文件或文件夹拖进窗口")
                    .small()
                    .color(egui::Color32::from_gray(120)),
            );
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("ffmpeg 目录：");
            ui.add(
                egui::TextEdit::singleline(&mut self.ffmpeg_dir)
                    .desired_width(260.0)
                    .hint_text("留空则从 PATH 查找"),
            );
            ui.label("needle 目录：");
            ui.add(
                egui::TextEdit::singleline(&mut self.needle_dir)
                    .desired_width(220.0)
                    .hint_text("留空则从 PATH 查找"),
            );
            if ui.button("重新检查工具").clicked() {
                let ff = non_empty(&self.ffmpeg_dir);
                let nd = non_empty(&self.needle_dir);
                self.status_line = "正在重新检查工具…".to_string();
                self.rx = Some(worker::spawn_check_tools(ui.ctx().clone(), ff, nd));
            }
        });
    }

    fn table_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong(format!("② 剧集列表（{} 个文件）", self.rows.len()));
            if !self.rows.is_empty() {
                let ready = self
                    .rows
                    .iter()
                    .filter(|r| r.task.kind.is_actionable())
                    .count();
                ui.label(
                    egui::RichText::new(format!("可切割 {ready} 集"))
                        .color(egui::Color32::from_rgb(20, 120, 50)),
                );
                let total_removed: f64 = self
                    .rows
                    .iter()
                    .filter(|r| r.task.kind.is_actionable())
                    .filter_map(|r| r.task.removed_secs())
                    .sum();
                if total_removed > 0.0 {
                    ui.label(
                        egui::RichText::new(format!("预计共去掉 {:.1} 分钟", total_removed / 60.0))
                            .color(egui::Color32::from_gray(120)),
                    );
                }
            }
        });

        if self.busy != Busy::Idle {
            ui.add(
                egui::ProgressBar::new(self.progress)
                    .show_percentage()
                    .text(self.stage.as_str()),
            );
            if self.counter.1 > 0 {
                ui.label(format!("进度：{}/{}", self.counter.0, self.counter.1));
            }
        }

        if self.rows.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("还没有导入文件。").color(egui::Color32::from_gray(120)));
            return;
        }

        let mut actions: Vec<RowAction> = Vec::new();

        // 先把 rows 整体换出来，绕开闭包里的借用冲突（见模块文档）
        let mut rows = std::mem::take(&mut self.rows);

        egui::ScrollArea::horizontal()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("episodes")
                    .num_columns(7)
                    .striped(true)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        // 表头
                        for h in [
                            "#",
                            "文件",
                            "片头（起 → 止）",
                            "片尾（起 → 止）",
                            "保留",
                            "状态",
                            "操作",
                        ] {
                            ui.label(egui::RichText::new(h).strong());
                        }
                        ui.end_row();

                        for (i, row) in rows.iter_mut().enumerate() {
                            ui.label(format!("{}", i + 1));

                            // 文件名 + 次要信息
                            ui.vertical(|ui| {
                                let name = row.task.file.name();
                                ui.label(egui::RichText::new(name.as_str()).strong())
                                    .on_hover_text(row.task.file.path.display().to_string());
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · {} · {}",
                                        row.task.file.display_size(),
                                        row.task.file.display_duration(),
                                        row.task.file.display_resolution()
                                    ))
                                    .small()
                                    .color(egui::Color32::from_gray(120)),
                                );
                                if !row.task.file.has_audio {
                                    ui.label(
                                        egui::RichText::new("无音轨，needle 无法分析")
                                            .small()
                                            .color(egui::Color32::from_rgb(185, 28, 28)),
                                    );
                                }
                            });

                            // 片头起止
                            ui.horizontal(|ui| {
                                let changed = timestamp_edit(ui, &mut row.opening_start);
                                ui.label("→");
                                let changed2 = timestamp_edit(ui, &mut row.opening_end);
                                if changed || changed2 {
                                    row.apply_text_edits();
                                }
                            });

                            // 片尾起止
                            ui.horizontal(|ui| {
                                let changed = timestamp_edit(ui, &mut row.ending_start);
                                ui.label("→");
                                let changed2 = timestamp_edit(ui, &mut row.ending_end);
                                if changed || changed2 {
                                    row.apply_text_edits();
                                }
                            });

                            // 保留区间
                            let keep_text = if row.task.keep.is_empty() {
                                "—".to_string()
                            } else {
                                pipeline::describe_keep(&row.task.keep)
                            };
                            let removed = row
                                .task
                                .removed_secs()
                                .map(|s| format!("去掉 {s:.1}s"))
                                .unwrap_or_default();
                            ui.vertical(|ui| {
                                ui.label(truncate(&keep_text, 30))
                                    .on_hover_text(keep_text.as_str());
                                if !removed.is_empty() {
                                    ui.label(
                                        egui::RichText::new(removed)
                                            .small()
                                            .color(egui::Color32::from_gray(120)),
                                    );
                                }
                                if let Some(err) = &row.edit_error {
                                    ui.label(
                                        egui::RichText::new(err.as_str())
                                            .small()
                                            .color(egui::Color32::from_rgb(185, 28, 28)),
                                    );
                                }
                            });

                            // 状态 + 这一集的进度
                            ui.vertical(|ui| {
                                let dark = ui.visuals().dark_mode;
                                let mut text = row.status.label();
                                if row.status == RowStatus::Idle && !row.task.kind.is_actionable() {
                                    text = row.task.kind.label().to_string();
                                }
                                let color = if row.status == RowStatus::Idle
                                    && !row.task.kind.is_actionable()
                                {
                                    egui::Color32::from_rgb(150, 100, 10)
                                } else {
                                    row.status.color(dark)
                                };
                                ui.label(egui::RichText::new(text).color(color));
                                if row.status == RowStatus::Cutting || row.progress > 0.0 {
                                    ui.add(
                                        egui::ProgressBar::new(row.progress).desired_width(120.0),
                                    );
                                }
                            });

                            // 操作
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(
                                        !row.preview.loading,
                                        egui::Button::new("预览").small(),
                                    )
                                    .clicked()
                                {
                                    actions.push(RowAction::Preview(i));
                                }
                                if ui
                                    .add_enabled(
                                        !row.task.detection.is_empty(),
                                        egui::Button::new("重置切点").small(),
                                    )
                                    .on_hover_text("丢弃手动修改，恢复成检测出来的原始时间戳")
                                    .clicked()
                                {
                                    actions.push(RowAction::ResetRow(i));
                                }
                                if !row.log.is_empty() {
                                    ui.label(
                                        egui::RichText::new("ⓘ")
                                            .color(egui::Color32::from_rgb(30, 90, 190)),
                                    )
                                    .on_hover_text(
                                        row.log.iter().cloned().collect::<Vec<_>>().join("\n"),
                                    );
                                }
                            });

                            ui.end_row();
                        }
                    });
            });

        self.rows = rows;

        // 表格渲染完再执行动作，避免遍历中改状态
        for action in actions {
            match action {
                RowAction::Preview(i) => self.request_preview(i),
                RowAction::ResetRow(i) => {
                    if let Some(row) = self.rows.get_mut(i) {
                        row.reset_texts_from_detection();
                        pipeline::recompute_task(&mut row.task);
                    }
                }
            }
        }
    }

    fn detect_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("③ 检测片头片尾");
            ui.separator();
            if ui
                .add_enabled(self.busy == Busy::Idle, egui::Button::new("开始检测"))
                .clicked()
            {
                self.start_detect();
            }
            ui.label(
                egui::RichText::new(format!("后端：{}", self.detector_label()))
                    .small()
                    .color(egui::Color32::from_gray(120)),
            );

            // 只有编译期开了 needle-lib 才给出这个选择；否则多一个勾选框只会让人困惑
            #[cfg(feature = "needle-lib")]
            {
                let before = self.prefer_library;
                ui.checkbox(&mut self.prefer_library, "优先用库直链")
                    .on_hover_text(
                        "库直链不产生 .needle.dat 临时文件，但拿不到逐集进度、\
                         也没法在阶段中途取消。切换后立即生效。",
                    );
                if before != self.prefer_library {
                    self.detector = worker::pick_detector(
                        self.tool_info.as_ref().and_then(|t| t.needle.as_ref()),
                        self.prefer_library,
                    );
                }
            }
        });

        egui::CollapsingHeader::new("检测参数（一般不用改）")
            .default_open(false)
            .show(ui, |ui| {
                ui.checkbox(&mut self.detect_opts.include_endings, "检测片尾")
                    .on_hover_text("关掉能省掉一遍视频尾段的解码，快一些；但片尾就不会被去掉");
                ui.checkbox(&mut self.detect_opts.threading, "启用多线程");

                ui.horizontal(|ui| {
                    ui.label("哈希匹配阈值：");
                    ui.add(
                        egui::DragValue::new(&mut self.detect_opts.hash_match_threshold)
                            .range(0..=32)
                            .speed(0.2),
                    )
                    .on_hover_text("0 = 完全相同，32 = 完全不同。调小更严格，但可能检不出");
                    ui.label(
                        egui::RichText::new("（越小越严格，默认 10）")
                            .small()
                            .color(egui::Color32::from_gray(120)),
                    );
                });

                ui.horizontal(|ui| {
                    ui.label("片头最短时长：");
                    ui.add(
                        egui::DragValue::new(&mut self.detect_opts.min_opening_duration_secs)
                            .range(1..=600)
                            .suffix(" 秒"),
                    )
                    .on_hover_text("设成接近真实片头长度能显著减少误报，默认 20 秒");
                    ui.label("片尾最短时长：");
                    ui.add(
                        egui::DragValue::new(&mut self.detect_opts.min_ending_duration_secs)
                            .range(1..=600)
                            .suffix(" 秒"),
                    );
                });

                ui.horizontal(|ui| {
                    ui.label("时间边距：");
                    ui.add(
                        egui::DragValue::new(&mut self.detect_opts.time_padding_secs)
                            .range(0.0..=30.0)
                            .speed(0.1)
                            .suffix(" 秒"),
                    )
                    .on_hover_text("片头起点往后、片尾终点往前各收这么多，用来抵消检测误差");
                    ui.label("逐集并发数：");
                    ui.add(
                        egui::DragValue::new(&mut self.detect_opts.analyze_parallelism)
                            .range(1..=16),
                    );
                });

                ui.checkbox(
                    &mut self.detect_opts.force_reanalyze,
                    "强制重新分析（忽略已有的 .needle.dat 缓存）",
                )
                .on_hover_text(
                    "如果之前用「只检测片头」跑过，缓存里没有片尾数据，再开片尾检测就会\
                     一直搜不到片尾 —— 勾上这个重算。",
                );
                ui.checkbox(
                    &mut self.detect_opts.keep_sidecars,
                    "保留 needle 生成的临时文件（.needle.dat / .needle.skip.json）",
                )
                .on_hover_text(
                    "默认会清掉这些工作产物，只在本次运行确实新建了它们时才删，\
                     你自己原有的缓存不会被动。",
                );
            });
    }

    fn output_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("④ 切割输出");
            ui.separator();
            let out_label = self
                .out_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "（未选择）".to_string());
            ui.label("输出目录：");
            ui.label(egui::RichText::new(truncate(&out_label, 60)).monospace())
                .on_hover_text(out_label.as_str());
            if ui.button("选择…").clicked() {
                self.dialog_out.pick_directory();
            }
            if self.out_dir.is_some() && ui.button("打开").clicked() {
                if let Some(dir) = self.out_dir.clone() {
                    open_in_file_manager(&dir);
                }
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("文件名后缀：");
            ui.add(egui::TextEdit::singleline(&mut self.out_suffix).desired_width(100.0));
            ui.label(
                egui::RichText::new("例：Show.S01E01_trimmed.mkv")
                    .small()
                    .color(egui::Color32::from_gray(120)),
            );
            ui.separator();
            ui.checkbox(&mut self.overwrite, "覆盖已存在的输出文件");
            ui.separator();
            ui.label("切割模式：");
            ui.selectable_value(&mut self.cut_mode, CutMode::Fast, "快速（流复制）")
                .on_hover_text(
                    "秒级完成，不重新编码、画质无损；起点会吸附到最近的关键帧，可能早 1～5 秒",
                );
            ui.selectable_value(&mut self.cut_mode, CutMode::Precise, "精确（重新编码）")
                .on_hover_text("帧精确；但要把正片整段重新编码，一集 45 分钟 1080p 可能要几分钟");
        });

        if self.cut_mode == CutMode::Fast {
            ui.label(
                egui::RichText::new(
                    "快速模式说明：起点会对齐到关键帧，所以切出来可能比标定时间早一点。\
                     如果发现正片开头被切掉，把片头终点往后调 1～3 秒，或者改用精确模式。",
                )
                .small()
                .color(egui::Color32::from_gray(120)),
            );
        }

        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.busy == Busy::Idle, egui::Button::new("开始切割"))
                .clicked()
            {
                self.start_cut();
            }
            ui.label(
                egui::RichText::new("源文件不会被修改，所有输出都写到上面的输出目录")
                    .small()
                    .color(egui::Color32::from_rgb(20, 120, 50)),
            );
        });
    }

    /// 拖放：接受文件与文件夹。
    ///
    /// 0.36 起 `DroppedFile` 是个 trait（`Arc<dyn DroppedFile>`），不再是带
    /// `path: Option<PathBuf>` 字段的结构体，所以要用 `f.path()` 取路径。
    /// 它返回的是 `&Path` 而不是 `Option`，拿不到路径的情况在原生平台上不存在
    /// （只有浏览器里路径会是相对的），所以这里不需要过滤。
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.add_inputs(dropped);
        }
    }

    /// 拖拽悬停时的提示层。
    fn drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering {
            return;
        }
        // `screen_rect` 已改名：`content_rect` 会避开刘海/状态栏之类被遮住的区域，
        // 而 `viewport_rect` 是整块视口。遮挡提示层用前者更准确。
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        painter.rect_filled(
            screen,
            egui::CornerRadius::ZERO,
            egui::Color32::from_rgba_unmultiplied(30, 90, 190, 40),
        );
        painter.text(
            screen.center(),
            egui::Align2::CENTER_CENTER,
            "松开即导入（支持文件与文件夹）",
            egui::FontId::proportional(26.0),
            egui::Color32::from_rgb(20, 60, 140),
        );
    }

    /// 处理两个文件对话框的选择结果。
    fn pump_file_dialogs(&mut self) {
        // 添加文件 / 文件夹
        let mut picked: Vec<PathBuf> = Vec::new();
        if let Some(paths) = self.dialog_add.take_picked_multiple() {
            picked.extend(paths);
        }
        if let Some(one) = self.dialog_add.take_picked() {
            picked.push(one);
        }
        if !picked.is_empty() {
            self.add_inputs(picked);
        }

        // 输出目录
        if let Some(one) = self.dialog_out.take_picked() {
            self.status_line = format!("输出目录：{}", one.display());
            self.out_dir = Some(one.clone());
            // 输出目录或后缀变了，已算好的输出路径要跟着变
            for row in &mut self.rows {
                row.task.output =
                    pipeline::output_path_for(&row.task.file.path, &one, &self.out_suffix);
            }
        }
        if let Some(one) = self.dialog_out.take_picked_multiple() {
            if let Some(first) = one.into_iter().next() {
                self.out_dir = Some(first);
            }
        }
    }
}

// ---------------------------------------------------------------------
// 自由函数
// ---------------------------------------------------------------------

/// 一个可编辑的时间戳输入框。
///
/// 宽度固定 96：`00:00:00.000` 刚好放得下，四列并排也不会把表格撑爆。
fn timestamp_edit(ui: &mut egui::Ui, text: &mut String) -> bool {
    let valid = text.trim().is_empty() || parse_timestamp(text).is_some();
    let mut edit = egui::TextEdit::singleline(text)
        .desired_width(96.0)
        .font(egui::TextStyle::Monospace)
        .hint_text("--:--:--");
    if !valid {
        // 解析不了就红框标出来，但不阻止继续输入（用户可能正打到一半）
        edit = edit.text_color(egui::Color32::from_rgb(185, 28, 28));
    }
    ui.add(edit).changed()
}

/// 解析一个时间戳字段。空串表示「这一段不存在」，返回 `Ok(None)`。
fn resolve_field(text: &str) -> std::result::Result<Option<f64>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    parse_timestamp(t)
        .map(Some)
        .ok_or_else(|| format!("「{t}」不是合法时间戳（可用 00:01:30 或 90.5 或 1:30）"))
}

/// 把一对（起, 止）合成区间；任一为空或顺序不对就返回 `None`。
fn join_segment(start: Option<f64>, end: Option<f64>) -> Option<Segment> {
    let (s, e) = (start?, end?);
    let seg = Segment::new(s, e);
    if seg.is_valid() {
        Some(seg)
    } else {
        // 起点比终点晚：这是笔误，宁可当成「这一段不存在」也不要切错
        None
    }
}

/// 把一个区间拆成（起文本, 止文本）。
fn split_text(seg: Option<Segment>) -> (String, String) {
    match seg {
        Some(s) => (format_timestamp(s.start), format_timestamp(s.end)),
        None => (String::new(), String::new()),
    }
}

/// 工具路径与版本的悬浮提示。
///
/// 顺手让 `tool_info` 字段在任何编译配置下都被读到 —— 否则不开 `needle-lib`
/// 时它只在写入端出现，会触发 dead_code 警告。
fn tool_tooltip(info: Option<&ToolInfo>) -> String {
    match info {
        Some(t) => format!(
            "ffmpeg：{}\n{}\nneedle：{}",
            t.ffmpeg.display(),
            t.ffmpeg_version,
            t.needle
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未找到".to_string())
        ),
        None => "正在检查…".to_string(),
    }
}

/// `ffmpeg version 7.1 Copyright...` -> `7.1`
fn short_version(raw: &str) -> String {
    raw.split_whitespace()
        .nth(2)
        .map(|s| s.to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// 把输入框内容转成 `Option<PathBuf>`：空或纯空白视为「没指定」。
fn non_empty(s: &str) -> Option<PathBuf> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(PathBuf::from(t))
    }
}

/// 太长的文本截断显示，完整内容放 tooltip。
fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 用系统文件管理器打开一个目录。
fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";

    match std::process::Command::new(cmd).arg(path).spawn() {
        Ok(_) => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "打开文件管理器失败"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_field_distinguishes_empty_from_invalid() {
        // 空串 = 这一段不存在，是合法输入
        assert_eq!(resolve_field(""), Ok(None));
        assert_eq!(resolve_field("   "), Ok(None));
        assert_eq!(resolve_field("00:01:30"), Ok(Some(90.0)));
        // 非空但解析不了必须报错，不能静默变成 None 或 0
        assert!(resolve_field("abc").is_err());
        assert!(resolve_field("12:99:99").is_ok()); // 92 分钟，语法上合法
    }

    #[test]
    fn join_segment_rejects_reversed_ranges() {
        assert_eq!(join_segment(None, Some(10.0)), None);
        assert_eq!(join_segment(Some(10.0), None), None);
        assert_eq!(join_segment(Some(10.0), Some(5.0)), None);
        assert_eq!(
            join_segment(Some(5.0), Some(10.0)),
            Some(Segment::new(5.0, 10.0))
        );
    }

    #[test]
    fn split_text_round_trips() {
        let (s, e) = split_text(Some(Segment::new(90.0, 130.0)));
        assert_eq!(s, "00:01:30.000");
        assert_eq!(e, "00:02:10.000");
        assert_eq!(resolve_field(&s).unwrap(), Some(90.0));
        assert_eq!(resolve_field(&e).unwrap(), Some(130.0));

        let (s, e) = split_text(None);
        assert!(s.is_empty() && e.is_empty());
    }

    #[test]
    fn short_version_extracts_the_number() {
        assert_eq!(
            short_version("ffmpeg version 7.1 Copyright (c) 2000-2024 the FFmpeg developers"),
            "7.1"
        );
        // 格式不符时原样返回，别丢掉信息
        assert_eq!(short_version("怪东西"), "怪东西");
    }

    #[test]
    fn truncate_handles_multibyte_text() {
        // 按字符而不是按字节截断，否则中文会被截成半个字
        assert_eq!(truncate("短", 10), "短");
        let long = "剧集片头片尾批量去除工具";
        assert_eq!(truncate(long, 5).chars().count(), 5);
        assert!(truncate(long, 5).ends_with('…'));
    }

    #[test]
    fn non_empty_treats_whitespace_as_absent() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty(" C:\\ffmpeg "), Some(PathBuf::from("C:\\ffmpeg")));
    }
}
