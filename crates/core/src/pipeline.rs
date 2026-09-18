//! 编排完整流程：扫目录 → 探测媒体信息 → 检测片头片尾 → 生成切割计划 → 批量切割。
//!
//! GUI 只跟这一层打交道。把编排放在 `core` 而不是 GUI 里有三个实际好处：
//! 未来加 CLI 可以直接复用；流程能在没有界面的情况下被集成测试；GUI 里那些
//! `match` 分支不会跟业务逻辑缠在一起。
//!
//! # 一条硬规则：源文件永不被改写
//!
//! 所有输出都写到用户单独指定的输出目录，文件名加统一后缀（默认 `_trimmed`）。
//! 源目录里除了 needle 的临时旁挂文件（跑完自动清理）之外，一个字节都不会变。
//! 这条规则由 [`build_tasks`] 在做计划时就保证，并且 [`crate::cut::Cutter`]
//! 还会在真正动手前再拦一道「输出路径 == 输入路径」。

use std::path::{Path, PathBuf};

use crate::cut::{CutMode, CutObserver, CutOutcome, Cutter};
use crate::detect::CancelFlag;
use crate::error::{CoreError, Result};
use crate::model::{keep_segments, Detection, EpisodeFile, EpisodeTask, Segment, TaskKind};
use crate::probe::FfmpegTools;

/// 会被当成「剧集视频」处理的扩展名。
///
/// 只列真正会用到的几种。把 `ts` / `flv` 也放进来是因为有些老资源是这些封装，
/// 但要注意 `ts` 在流复制切割时对关键帧更敏感。
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "m4v", "webm", "ts", "wmv", "flv", "mpg", "mpeg",
];

/// 输出文件的默认后缀。
pub const DEFAULT_OUTPUT_SUFFIX: &str = "_trimmed";

/// 判断一个路径是不是受支持的视频文件（只看扩展名，不看内容）。
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or(false)
}

/// 扫描一批路径（文件或目录），收集所有视频文件。
///
/// 返回结果**已排序**：剧集文件名基本都是 `S01E01` 这种零填充命名，字典序
/// 就是正确的播放顺序，排序后用户的列表看起来才顺眼。另外 needle 的跨集比对
/// 也是按传入顺序配对的，稳定的顺序让结果可复现。
///
/// `recursive` 为 false 时只看一层目录；为 true 时递归。两种情况下都会跳过
/// 隐藏项（以 `.` 开头）—— 我们自己造的临时目录就是隐藏的，不该被扫回来。
pub fn scan_videos(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_file() {
            if is_video_file(p) {
                out.push(p.clone());
            }
        } else if p.is_dir() {
            collect_videos(p, recursive, &mut out)?;
        } else {
            // 拖进来的路径可能已经不存在了（用户拖完又删了），跳过比报错友好
            tracing::warn!(path = %p.display(), "路径不存在，已跳过");
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_videos(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "目录读不了，已跳过");
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            if recursive {
                collect_videos(&path, recursive, out)?;
            }
        } else if is_video_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

/// 逐个用 ffprobe 探测媒体信息。
///
/// 一集探测失败不会中断整批 —— 一个坏文件不该让用户白等一场。
/// 失败的那些仍然会进结果列表，只是 `duration_secs` 为 `None`，
/// 后续 [`build_tasks`] 会把它标成 `DurationUnknown` 并在界面上说清楚原因。
pub fn probe_all(
    tools: &FfmpegTools,
    paths: &[PathBuf],
    cancel: &CancelFlag,
    on_progress: &mut dyn FnMut(usize, usize, &str),
) -> Result<Vec<EpisodeFile>> {
    let total = paths.len();
    let mut out = Vec::with_capacity(total);

    for (i, path) in paths.iter().enumerate() {
        if crate::detect::check_cancel(cancel).is_err() {
            return Err(CoreError::Cancelled);
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        on_progress(i + 1, total, &name);

        let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let (duration_secs, width, height, has_audio) = match tools.probe(path) {
            Ok(info) => (
                Some(info.duration_secs),
                info.width,
                info.height,
                info.has_audio,
            ),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "探测失败");
                (None, None, None, true)
            }
        };

        out.push(EpisodeFile {
            path: path.clone(),
            size_bytes,
            duration_secs,
            width,
            height,
            has_audio,
        });
    }

    Ok(out)
}

/// 按「输入文件名 + 后缀 + 原扩展名」算出输出路径。
///
/// 保留原扩展名很重要：mkv 里可能塞着字幕和章节，输出成 mp4 会直接丢掉它们。
pub fn output_path_for(input: &Path, out_dir: &Path, suffix: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let ext = input
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mkv".to_string());
    out_dir.join(format!("{stem}{suffix}.{ext}"))
}

/// 把「探测结果 + 检测结果」合成一份可执行的切割计划。
///
/// 每一集都会被分类成 [`TaskKind`]，只有 `Ready` 的才会真的送进 ffmpeg。
/// 分类而不是报错，是因为「某集没检测到片头」和「某集时长读不出来」都是
/// 正常会遇到的情况，用户需要看到一整个列表里哪几集有问题、为什么。
pub fn build_tasks(
    files: &[EpisodeFile],
    detections: &[Detection],
    out_dir: &Path,
    suffix: &str,
) -> Vec<EpisodeTask> {
    files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            // `get` 而不是下标：调用方给的检测结果可能比文件少（理论上不该发生），
            // 少的时候按「没检测到」处理，而不是 panic 掉整个界面
            let detection = detections.get(i).cloned().unwrap_or_default();

            let (keep, kind) = match file.duration_secs {
                None => (Vec::new(), TaskKind::DurationUnknown),
                Some(duration) => {
                    let keep = keep_segments(duration, &detection);
                    let kind = if keep.is_empty() {
                        TaskKind::NothingToKeep
                    } else if detection.is_empty() {
                        TaskKind::NoDetection
                    } else {
                        TaskKind::Ready
                    };
                    (keep, kind)
                }
            };

            EpisodeTask {
                output: output_path_for(&file.path, out_dir, suffix),
                file: file.clone(),
                detection,
                keep,
                kind,
            }
        })
        .collect()
}

/// 重算某一集的保留区间。
///
/// 用户在界面上手改了时间戳之后要调这个 —— 改了检测结果就得重新算切割方案，
/// 否则切出来的还是按旧时间戳算的段落。
pub fn recompute_task(task: &mut EpisodeTask) {
    let Some(duration) = task.file.duration_secs else {
        task.kind = TaskKind::DurationUnknown;
        task.keep.clear();
        return;
    };
    let keep = keep_segments(duration, &task.detection);
    task.kind = if keep.is_empty() {
        TaskKind::NothingToKeep
    } else if task.detection.is_empty() {
        TaskKind::NoDetection
    } else {
        TaskKind::Ready
    };
    task.keep = keep;
}

/// 批量切割过程中接收进度的回调集合。
///
/// 做成 trait 而不是一串 `&mut dyn FnMut`，是因为这里有三个语义不同的回调
/// （日志 / 进度 / 完成），平铺成参数会让函数签名长到看不清，而且 GUI 侧
/// 实现这个 trait 比同时捕获三个闭包更自然。
pub trait CutSink {
    /// ffmpeg 的一行可读输出
    fn line(&mut self, task_index: usize, line: &str);
    /// 某一集的完成度，0.0 ~ 1.0
    fn progress(&mut self, task_index: usize, fraction: f64);
    /// 某一集跑完了（成功或失败都会调）
    fn finished(&mut self, task_index: usize, outcome: &Result<CutOutcome>);
}

/// 把「批次级的 [`CutSink`]」适配成「单次切割的 [`CutObserver`]」。
///
/// 存在的唯一理由是借用关系：[`CutSink`] 的方法都带 `task_index`，而在
/// `Cutter::cut` 内部只有「某一集」的概念。适配器把下标绑进来，顺带让
/// `sink` 的可变借用收敛成一个对象 —— 直接传两个闭包会同时可变借同一份
/// sink，过不了借用检查。
struct SinkObserver<'a> {
    sink: &'a mut dyn CutSink,
    index: usize,
}

impl CutObserver for SinkObserver<'_> {
    fn line(&mut self, line: &str) {
        self.sink.line(self.index, line);
    }

    fn progress(&mut self, fraction: f64) {
        self.sink.progress(self.index, fraction);
    }
}

/// 批量切割的汇总。
#[derive(Debug, Clone, Default)]
pub struct CutSummary {
    pub succeeded: usize,
    pub failed: usize,
    /// 因为不需要切 / 时长未知而跳过的
    pub skipped: usize,
    /// 成功切出来的总时长（秒）
    pub produced_secs: f64,
}

/// 依次切割所有可执行的任务。
///
/// 刻意**串行**执行：切割是磁盘 IO 密集型的，并发跑反而让机械硬盘来回寻道，
/// 总耗时更长；而且串行时进度条的「第 X/Y 集」才是有意义的。
/// 性能瓶颈本来就在 ffmpeg 的流复制速度上，串行已经能跑满磁盘带宽。
pub fn cut_tasks(
    cutter: &Cutter,
    tasks: &[EpisodeTask],
    mode: CutMode,
    overwrite: bool,
    cancel: &CancelFlag,
    sink: &mut dyn CutSink,
) -> CutSummary {
    let mut summary = CutSummary::default();

    for (i, task) in tasks.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !task.kind.is_actionable() {
            summary.skipped += 1;
            sink.line(i, &format!("跳过（{}）", task.kind.label()));
            continue;
        }

        let req = crate::cut::CutRequest {
            input: task.file.path.clone(),
            output: task.output.clone(),
            keep: task.keep.clone(),
            mode,
            overwrite,
        };

        // 单独一个作用域：适配器借走 `sink`，出块就还回来，
        // 后面还要用它调 `finished`
        let outcome = {
            let mut observer = SinkObserver {
                sink: &mut *sink,
                index: i,
            };
            cutter.cut(&req, Some(cancel), &mut observer)
        };

        match &outcome {
            Ok(o) => {
                summary.succeeded += 1;
                summary.produced_secs += o.kept_secs;
            }
            Err(CoreError::Cancelled) => {
                summary.failed += 1;
                sink.finished(i, &outcome);
                break;
            }
            Err(_) => {
                summary.failed += 1;
            }
        }
        sink.finished(i, &outcome);
    }

    summary
}

/// 计划里所有可执行任务的总时长，用于计算整批的总体进度。
pub fn total_actionable_secs(tasks: &[EpisodeTask]) -> f64 {
    tasks
        .iter()
        .filter(|t| t.kind.is_actionable())
        .filter_map(|t| t.kept_secs())
        .sum()
}

/// 需要保留的区间文本，形如 `00:02:12.000 - 00:22:10.000`；多段用 ` + ` 连接。
pub fn describe_keep(keep: &[Segment]) -> String {
    keep.iter()
        .map(|s| s.display())
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_supported_extensions_case_insensitively() {
        assert!(is_video_file(Path::new("/tv/a.MKV")));
        assert!(is_video_file(Path::new("/tv/a.mp4")));
        // needle 的旁挂文件不能被当成视频扫进来
        assert!(!is_video_file(Path::new("/tv/a.needle.skip.json")));
        assert!(!is_video_file(Path::new("/tv/a.needle.dat")));
        assert!(!is_video_file(Path::new("/tv/readme.txt")));
    }

    #[test]
    fn output_path_keeps_the_original_container() {
        let out = output_path_for(
            Path::new(r"C:\TV\Show\Show.S01E01.mkv"),
            Path::new(r"D:\out"),
            DEFAULT_OUTPUT_SUFFIX,
        );
        assert_eq!(out, PathBuf::from(r"D:\out\Show.S01E01_trimmed.mkv"));

        // 没有扩展名的输入也不能产出没有扩展名的输出
        let out = output_path_for(Path::new("/tv/raw"), Path::new("/out"), "_c");
        assert_eq!(out, PathBuf::from("/out/raw_c.mkv"));
    }

    fn episode(name: &str, duration: Option<f64>) -> EpisodeFile {
        EpisodeFile {
            path: PathBuf::from(format!("/tv/{name}")),
            size_bytes: 1024,
            duration_secs: duration,
            width: Some(1920),
            height: Some(1080),
            has_audio: true,
        }
    }

    #[test]
    fn classifies_each_episode_into_an_actionable_or_explainable_kind() {
        let files = vec![
            episode("e01.mkv", Some(1440.0)),
            episode("e02.mkv", Some(1440.0)),
            episode("e03.mkv", None),
            episode("e04.mkv", Some(1440.0)),
        ];
        let detections = vec![
            // 有片头有片尾、且正好占住头尾 -> 可切，恰好留一段
            Detection {
                opening: Some(Segment::new(0.0, 132.0)),
                ending: Some(Segment::new(1331.0, 1440.0)),
            },
            // 什么都没检测到 -> 无需切割
            Detection::default(),
            // 时长读不出来 -> 无法计算，即使检测结果正常
            Detection {
                opening: Some(Segment::new(43.0, 132.0)),
                ending: None,
            },
            // 片头覆盖整段 -> 没有可保留内容
            Detection {
                opening: Some(Segment::new(0.0, 1440.0)),
                ending: None,
            },
        ];

        let tasks = build_tasks(&files, &detections, Path::new("/out"), "_trimmed");
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].kind, TaskKind::Ready);
        assert_eq!(tasks[0].keep.len(), 1);
        assert_eq!(tasks[1].kind, TaskKind::NoDetection);
        assert_eq!(tasks[2].kind, TaskKind::DurationUnknown);
        assert_eq!(tasks[3].kind, TaskKind::NothingToKeep);

        // 只有 Ready 的会被算进总时长
        let total = total_actionable_secs(&tasks);
        assert!((total - (1331.0 - 132.0)).abs() < 1e-6);
    }

    #[test]
    fn cold_open_and_trailing_preview_make_three_spans() {
        // 真实剧集的常见形态：冷开场 / 片头序列 / 正片 / 片尾序列 / 下集预告。
        // 检测器只点出中间那两个序列，两侧内容必须留下 —— 于是产生 3 段，
        // 走多段拼接路径。这条断言锁住这个语义，防止以后被「优化」成砍头去尾。
        let files = vec![episode("e01.mkv", Some(1440.0))];
        let detections = vec![Detection {
            opening: Some(Segment::new(43.0, 132.0)),
            ending: Some(Segment::new(1331.0, 1419.0)),
        }];
        let tasks = build_tasks(&files, &detections, Path::new("/out"), "_trimmed");

        assert_eq!(tasks[0].kind, TaskKind::Ready);
        assert_eq!(tasks[0].keep.len(), 3);
        assert_eq!(describe_keep(&tasks[0].keep).split(" + ").count(), 3);
    }

    #[test]
    fn missing_detection_entries_do_not_panic() {
        // 检测结果比文件少时按「未检测到」处理，不能让界面崩掉
        let files = vec![episode("e01.mkv", Some(1440.0))];
        let tasks = build_tasks(&files, &[], Path::new("/out"), "_trimmed");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, TaskKind::NoDetection);
    }

    #[test]
    fn recompute_task_reflects_manual_timestamp_edits() {
        let mut tasks = build_tasks(
            &[episode("e01.mkv", Some(1440.0))],
            &[Detection::default()],
            Path::new("/out"),
            "_trimmed",
        );
        let mut task = tasks.pop().unwrap();
        assert_eq!(task.kind, TaskKind::NoDetection);

        // 用户在界面上手填了片头片尾
        task.detection = Detection {
            opening: Some(Segment::new(60.0, 150.0)),
            ending: Some(Segment::new(1300.0, 1400.0)),
        };
        recompute_task(&mut task);

        assert_eq!(task.kind, TaskKind::Ready);
        // 手填 60~150 与 1300~1400，两侧的 0~60 与 1400~1440 是正片，共 3 段
        assert_eq!(task.keep.len(), 3);
        assert!((task.keep[0].start - 0.0).abs() < 1e-9);
        assert!((task.keep[0].end - 60.0).abs() < 1e-9);
        assert!((task.keep[1].start - 150.0).abs() < 1e-9);
        assert!((task.keep[1].end - 1300.0).abs() < 1e-9);
        assert!((task.keep[2].start - 1400.0).abs() < 1e-9);
        assert!((task.keep[2].end - 1440.0).abs() < 1e-9);
        // 切掉 90 + 100 = 190 秒
        assert!((task.removed_secs().unwrap() - 190.0).abs() < 1e-6);
    }

    #[test]
    fn describe_keep_joins_multiple_spans() {
        let keep = vec![Segment::new(0.0, 10.0), Segment::new(200.0, 300.0)];
        assert_eq!(
            describe_keep(&keep),
            "00:00:00.000 - 00:00:10.000 + 00:03:20.000 - 00:05:00.000"
        );
    }
}
