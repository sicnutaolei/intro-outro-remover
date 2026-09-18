//! 共享数据模型。
//!
//! 这一层只放**纯数据 + 纯计算**，不碰文件系统也不调外部命令，因此全部可单元测试。
//! GUI、切割、检测三条线共用这里的类型，避免各自造一套时间戳表示。

use std::path::PathBuf;

/// 判定「保留区间」时，短于这个长度的碎片直接丢掉。
///
/// 0.5 秒这个值来自实际场景：用户手改时间戳时很容易把片尾起点改到离片头终点
/// 只差零点几秒，结果切出一段几乎没内容的中间段 —— 那是误操作，不是需求。
pub const MIN_KEEP_SECS: f64 = 0.5;

/// 一个待处理的视频文件，以及从 ffprobe 探测到的媒体信息。
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeFile {
    /// 绝对路径
    pub path: PathBuf,
    /// 文件字节数
    pub size_bytes: u64,
    /// 时长（秒）。探测失败时为 `None` —— 注意这里不是 0，0 会被误当成「零长度视频」
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 是否含音轨。false 的文件送进 needle 只会报错，应当提前告诉用户
    pub has_audio: bool,
}

impl EpisodeFile {
    /// 只取文件名，用于列表显示。
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// 人类可读的文件大小。
    pub fn display_size(&self) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;
        let b = self.size_bytes as f64;
        if b >= GB {
            format!("{:.2} GB", b / GB)
        } else if b >= MB {
            format!("{:.1} MB", b / MB)
        } else if b >= KB {
            format!("{:.0} KB", b / KB)
        } else {
            format!("{} B", self.size_bytes)
        }
    }

    /// 人类可读的时长。
    pub fn display_duration(&self) -> String {
        match self.duration_secs {
            Some(d) => format_timestamp(d),
            None => "未知".to_string(),
        }
    }

    /// `1920x1080` 这样的分辨率文本。
    pub fn display_resolution(&self) -> String {
        match (self.width, self.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => "未知".to_string(),
        }
    }
}

/// 时间区间，单位秒（f64，够到毫秒精度）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

impl Segment {
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    /// 区间长度（秒）。不保证非负。
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }

    /// 是否是合法的正向区间：两端有限，且结束确实晚于开始。
    pub fn is_valid(&self) -> bool {
        self.start.is_finite() && self.end.is_finite() && self.end > self.start
    }

    /// `00:01:23.456 - 00:02:45.678`
    pub fn display(&self) -> String {
        format!(
            "{} - {}",
            format_timestamp(self.start),
            format_timestamp(self.end)
        )
    }
}

/// 单集的检测结果。
///
/// 两个字段都是 `Option`，因为 needle 对「没找到」和「找到了」是明确区分的：
/// 某集没有公共片头是正常情况（第一集经常没有），不该当成错误。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detection {
    pub opening: Option<Segment>,
    pub ending: Option<Segment>,
}

impl Detection {
    pub fn is_empty(&self) -> bool {
        self.opening.is_none() && self.ending.is_none()
    }
}

/// 单集的任务类型。决定 GUI 上这一行显示什么状态、以及要不要真的去切。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// 有内容要切，可以执行
    Ready,
    /// 检测出来没有片头也没有片尾，无需切割
    NoDetection,
    /// 片头片尾覆盖了整段视频，没有可保留内容
    NothingToKeep,
    /// 读不出时长，无法计算切割点
    DurationUnknown,
}

impl TaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Ready => "待切割",
            TaskKind::NoDetection => "未检测到片头片尾",
            TaskKind::NothingToKeep => "无可保留内容",
            TaskKind::DurationUnknown => "时长未知",
        }
    }

    /// 只有 `Ready` 的任务会被真正送进 ffmpeg。
    pub fn is_actionable(&self) -> bool {
        matches!(self, TaskKind::Ready)
    }
}

/// 一集的完整处理任务：输入、检测结果、要保留的区间、输出路径。
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeTask {
    pub file: EpisodeFile,
    pub detection: Detection,
    /// 要保留的区间（升序、互不重叠）。由 [`keep_segments`] 算出
    pub keep: Vec<Segment>,
    pub output: PathBuf,
    pub kind: TaskKind,
}

impl EpisodeTask {
    /// 保留内容的总时长。时长未知时返回 `None`。
    pub fn kept_secs(&self) -> Option<f64> {
        self.file
            .duration_secs
            .map(|_| self.keep.iter().map(|s| s.duration()).sum())
    }

    /// 被切掉的秒数。
    pub fn removed_secs(&self) -> Option<f64> {
        match (self.file.duration_secs, self.kept_secs()) {
            (Some(total), Some(keep)) => Some((total - keep).max(0.0)),
            _ => None,
        }
    }
}

/// 把秒数格式化为 `HH:MM:SS.mmm`。
///
/// 负数、NaN、无穷全部返回 `--:--:--`：这些都是「数据没拿到」的表示，
/// 显示成 `00:00:00` 会被用户误读成「从第 0 秒开始」。
pub fn format_timestamp(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--:--".to_string();
    }
    let total_ms = (secs * 1000.0).round() as u64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// 把秒数格式化为 `HH:MM:SS`（不带毫秒），用于紧凑显示。
pub fn format_clock(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--:--".to_string();
    }
    let total = secs.round() as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total / 60) % 60,
        total % 60
    )
}

/// 解析用户手输的时间戳。
///
/// 接受三种写法，因为提示词里就要求「HH:MM:SS 或秒数」都能用：
/// - `HH:MM:SS` / `HH:MM:SS.mmm` / `MM:SS`（缺省小时按 0 算）
/// - `90.5` / `90`（纯秒数）
/// - 前后空白、全角冒号会被容错处理 —— 中文输入法打出的 `：` 是常见情况
///
/// 解析不出来返回 `None`。**注意不要用 `unwrap_or(0.0)` 兜底**：把用户的笔误
/// 静默当成「第 0 秒」比直接报错危险得多。
pub fn parse_timestamp(input: &str) -> Option<f64> {
    let s = input.trim().replace('：', ":").replace(['，', ','], ".");
    if s.is_empty() {
        return None;
    }

    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        // 只接受 2 段（MM:SS）或 3 段（HH:MM:SS）
        if parts.len() < 2 || parts.len() > 3 {
            return None;
        }
        let mut secs = 0.0_f64;
        for (idx, part) in parts.iter().enumerate() {
            let part = part.trim();
            // 只有最后一段允许带小数秒
            let is_last = idx == parts.len() - 1;
            let value: f64 = if part.is_empty() {
                return None;
            } else if is_last {
                part.parse().ok()?
            } else {
                // 中间段必须是整数，否则 "1.5:30" 这种会被曲解成 90+30
                let v: u64 = part.parse().ok()?;
                v as f64
            };
            secs = secs * 60.0 + value;
        }
        if secs.is_finite() && secs >= 0.0 {
            Some(secs)
        } else {
            None
        }
    } else {
        let v: f64 = s.parse().ok()?;
        if v.is_finite() && v >= 0.0 {
            Some(v)
        } else {
            None
        }
    }
}

/// 由检测结果与视频总时长，算出**要保留**的区间序列。
///
/// 做法是先收集所有要切掉的区间（片头、片尾），排序合并，再取补集。
/// 之所以要合并而不是简单地把四个数拼起来：
/// 用户手改时间戳时可能把片尾起点改到片头终点之前，产生重叠区间；
/// 不合并就会算出负长度的「保留段」，送进 ffmpeg 直接报错。
///
/// # 片头之前、片尾之后的内容会被保留
///
/// 检测器给出的是**片头序列**和**片尾序列**各自占据的区间，不是「从第 0 秒到
/// 片头结束」这种前缀。两者之间、以及区间之外的部分是正片内容（冷开场、正片
/// 本体、下集预告），工具不替用户决定删掉它 —— 检测器没说要删的东西就保留。
/// 所以「掐头去尾」在真实剧集上通常是 **3 段**而不是 1 段，走 [`crate::cut`]
/// 的多段拼接路径。只有片头正好从 0 开始、片尾正好延续到视频结尾时才退化成 1 段。
///
/// 用户若想连下集预告一起去掉，在界面表格里把片尾终点改成视频结尾即可，
/// 时间戳是可编辑的。
pub fn keep_segments(duration: f64, detection: &Detection) -> Vec<Segment> {
    if !duration.is_finite() || duration <= 0.0 {
        return Vec::new();
    }

    // 1. 收集要切掉的区间，并夹到 [0, duration] 之内
    let mut cuts: Vec<Segment> = [detection.opening, detection.ending]
        .into_iter()
        .flatten()
        .map(|s| Segment::new(s.start.clamp(0.0, duration), s.end.clamp(0.0, duration)))
        .filter(|s| s.is_valid())
        .collect();

    if cuts.is_empty() {
        // 什么都不用切 —— 整段保留
        return vec![Segment::new(0.0, duration)];
    }

    // 2. 按起点排序后合并重叠区间
    cuts.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut merged: Vec<Segment> = Vec::with_capacity(cuts.len());
    for c in cuts {
        match merged.last_mut() {
            Some(last) if c.start <= last.end => {
                if c.end > last.end {
                    last.end = c.end;
                }
            }
            _ => merged.push(c),
        }
    }

    // 3. 取补集
    let mut keep: Vec<Segment> = Vec::new();
    let mut cursor = 0.0_f64;
    for c in &merged {
        if c.start > cursor {
            keep.push(Segment::new(cursor, c.start));
        }
        cursor = c.end;
    }
    if cursor < duration {
        keep.push(Segment::new(cursor, duration));
    }

    // 4. 丢掉过短的碎片（见 MIN_KEEP_SECS 的说明）
    keep.retain(|s| s.duration() > MIN_KEEP_SECS);
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_formatting_pads_and_handles_garbage() {
        assert_eq!(format_timestamp(0.0), "00:00:00.000");
        assert_eq!(format_timestamp(61.5), "00:01:01.500");
        assert_eq!(format_timestamp(3661.25), "01:01:01.250");
        // 没拿到数据时必须是显式的占位符，不能伪装成 0
        assert_eq!(format_timestamp(-1.0), "--:--:--");
        assert_eq!(format_timestamp(f64::NAN), "--:--:--");
        assert_eq!(format_timestamp(f64::INFINITY), "--:--:--");
    }

    #[test]
    fn timestamp_parsing_accepts_all_three_documented_forms() {
        // 纯秒数
        assert_eq!(parse_timestamp("90"), Some(90.0));
        assert_eq!(parse_timestamp("90.5"), Some(90.5));
        // MM:SS
        assert_eq!(parse_timestamp("01:30"), Some(90.0));
        assert_eq!(parse_timestamp("1:30"), Some(90.0));
        // HH:MM:SS
        assert_eq!(parse_timestamp("00:01:30"), Some(90.0));
        assert_eq!(parse_timestamp("01:00:00"), Some(3600.0));
        assert_eq!(parse_timestamp("01:01:01.250"), Some(3661.25));
        // 中文输入法的全角冒号、前后空白
        assert_eq!(parse_timestamp(" 00：01：30 "), Some(90.0));
        // 小数点被写成中文逗号
        assert_eq!(parse_timestamp("90，5"), Some(90.5));
    }

    #[test]
    fn timestamp_parsing_rejects_garbage_instead_of_returning_zero() {
        // 这些都必须返回 None。返回 0.0 会让用户以为「从第 0 秒开始」而静默切错。
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("abc"), None);
        assert_eq!(parse_timestamp("12:34:56:78"), None); // 段数太多
        assert_eq!(parse_timestamp(":"), None);
        assert_eq!(parse_timestamp("1.5:30"), None); // 中间段带小数
        assert_eq!(parse_timestamp("-5"), None);
    }

    #[test]
    fn keep_segments_with_both_ends_removed_is_one_span() {
        // 片头就在最开头、片尾一直到文件结尾（无冷开场、无下集预告的剧集），
        // 结果恰好一段 —— 这是走「单条 ffmpeg 命令」快路径的判据。
        let d = Detection {
            opening: Some(Segment::new(0.0, 132.0)),
            ending: Some(Segment::new(1331.0, 1440.0)),
        };
        let keep = keep_segments(1440.0, &d);
        assert_eq!(keep.len(), 1);
        assert!((keep[0].start - 132.0).abs() < 1e-9);
        assert!((keep[0].end - 1331.0).abs() < 1e-9);
    }

    #[test]
    fn keep_segments_keeps_the_cold_open_and_the_tail() {
        // 有冷开场 + 片尾后还有下集预告的情况。检出的是「片头序列」和「片尾序列」
        // 各自占据的区间，区间之外是正片内容 —— 必须保留。
        // 这条语义是刻意的：检测器没说要删的东西，工具不替用户决定删掉。
        // 用户若想把下集预告也去掉，在界面表格里把片尾终点改成视频结尾即可。
        let d = Detection {
            opening: Some(Segment::new(43.0, 132.0)),
            ending: Some(Segment::new(1331.0, 1419.0)),
        };
        let keep = keep_segments(1440.0, &d);
        assert_eq!(keep.len(), 3);
        assert!((keep[0].start - 0.0).abs() < 1e-9); // 冷开场
        assert!((keep[0].end - 43.0).abs() < 1e-9);
        assert!((keep[1].start - 132.0).abs() < 1e-9); // 正片
        assert!((keep[1].end - 1331.0).abs() < 1e-9);
        assert!((keep[2].start - 1419.0).abs() < 1e-9); // 下集预告
        assert!((keep[2].end - 1440.0).abs() < 1e-9);
    }

    #[test]
    fn keep_segments_with_no_detection_keeps_whole_video() {
        let keep = keep_segments(1440.0, &Detection::default());
        assert_eq!(keep, vec![Segment::new(0.0, 1440.0)]);
    }

    #[test]
    fn keep_segments_merges_overlapping_edits() {
        // 用户把片尾起点改到片头终点之前 —— 两个区间重叠，必须合并成一段
        let d = Detection {
            opening: Some(Segment::new(10.0, 100.0)),
            ending: Some(Segment::new(50.0, 200.0)),
        };
        let keep = keep_segments(300.0, &d);
        // 切口是 [10,200]，保留 [0,10] 与 [200,300]
        assert_eq!(keep.len(), 2);
        assert!((keep[0].start - 0.0).abs() < 1e-9);
        assert!((keep[0].end - 10.0).abs() < 1e-9);
        assert!((keep[1].start - 200.0).abs() < 1e-9);
        assert!((keep[1].end - 300.0).abs() < 1e-9);
    }

    #[test]
    fn keep_segments_clamps_out_of_range_edits() {
        // 片头起点被写成负数、片尾终点超出视频长度，都要夹住而不是算出负数长度
        let d = Detection {
            opening: Some(Segment::new(-30.0, 60.0)),
            ending: Some(Segment::new(1380.0, 9999.0)),
        };
        let keep = keep_segments(1440.0, &d);
        assert_eq!(keep.len(), 1);
        assert!((keep[0].start - 60.0).abs() < 1e-9);
        assert!((keep[0].end - 1380.0).abs() < 1e-9);
    }

    #[test]
    fn keep_segments_drops_tiny_fragments() {
        // 保留段只剩 0.2 秒 —— 是笔误，不是需求，应当整体丢弃
        let d = Detection {
            opening: Some(Segment::new(0.0, 100.0)),
            ending: Some(Segment::new(100.2, 300.0)),
        };
        let keep = keep_segments(300.0, &d);
        assert!(keep.is_empty(), "低于 MIN_KEEP_SECS 的碎片应被丢弃");
    }

    #[test]
    fn keep_segments_rejects_bad_duration() {
        assert!(keep_segments(0.0, &Detection::default()).is_empty());
        assert!(keep_segments(f64::NAN, &Detection::default()).is_empty());
    }

    #[test]
    fn only_ready_tasks_are_actionable() {
        assert!(TaskKind::Ready.is_actionable());
        for k in [
            TaskKind::NoDetection,
            TaskKind::NothingToKeep,
            TaskKind::DurationUnknown,
        ] {
            assert!(!k.is_actionable(), "{k:?} 不应该被送进 ffmpeg");
        }
    }
}
