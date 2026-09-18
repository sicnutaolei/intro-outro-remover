//! 用 ffmpeg 切割视频：掐掉检测出来的片头 / 片尾，把剩下的接成新文件。
//!
//! # 两种模式的取舍（这是本模块最需要讲清楚的事）
//!
//! 关键在 `-ss` 放在 `-i` 前面还是后面，以及要不要重新编码。ffmpeg 文档里
//! 对 `-ss` 的两种位置有明确定义：
//!
//! - **`-ss` 作为输入选项（放在 `-i` 之前）**：seek 到「不晚于该时间点的最近
//!   关键帧」。如果同时在转码，且 `-accurate_seek` 生效（默认开），那么从关键帧
//!   到目标时间点之间的内容会被解码后丢弃，结果**帧精确**。但如果是**流复制**，
//!   这段多出来的内容会被保留 —— 也就是输出会从关键帧开始，比目标时间点早。
//! - **`-ss` 作为输出选项（放在 `-i` 之后）**：从头解码并丢帧到目标位置，
//!   帧精确，但代价是要解码前面所有内容。
//!
//! 由此得到本模块的两种模式：
//!
//! | 模式 | 命令形态 | 精度 | 速度 | 画质 |
//! |---|---|---|---|---|
//! | [`CutMode::Fast`] | `-ss 前 + -t + -c copy` | 起点吸附到关键帧，可能早 1～5 秒 | 秒级 | 无损（不重编码） |
//! | [`CutMode::Precise`] | `-ss 前 + -t + 重编码` | 帧精确 | 与视频时长成正比 | 有损（libx264 CRF 18） |
//!
//! 默认选 `Fast`：掐片头片尾这种场景，起点早一两秒完全可接受 —— 而 `Precise`
//! 要把整集重新编码一遍，一集 45 分钟 1080p 在普通 CPU 上要几分钟，一个季度
//! 就是几小时。真要精确，用 `--time-padding` 把切口往前挪一点比重新编码划算得多。
//!
//! # 为什么用 `-t` 而不是 `-to`
//!
//! `-to` 的语义会随 `-ss` 的位置变化（是相对原片还是相对 seek 点），这是 ffmpeg
//! 最容易踩的坑之一。`-t` 始终表示「输出时长」，配合我们自己算好的
//! `end - start` 就没有歧义。
//!
//! # 多段切割
//!
//! 片头在开头、片尾在结尾，所以正常情况（两个都切）只会留下**一段**，走单次
//! ffmpeg 调用即可。只有用户把片尾起点改到片头终点之前（误操作）或只切中间某段
//! 时才可能有多段，这时逐段输出到临时文件，再用 concat 分离器拼起来 —— 全程
//! `-c copy`，不重新编码。临时文件放在输出目录下的隐藏子目录里，跑完即删。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{CoreError, Result};
use crate::exec::{no_window, run_streaming, Stream};
use crate::model::{format_timestamp, Segment};

use crate::detect::CancelFlag;

/// 精确模式下重编码用的 CRF。18 是视觉无损附近的值。
const PRECISE_CRF: u32 = 18;
/// 精确模式下的重编码速度档。medium 是质量/速度的平衡点。
const PRECISE_PRESET: &str = "medium";

/// 切割模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutMode {
    /// 流复制，秒级完成，起止会吸附到关键帧。
    Fast,
    /// 重新编码，帧精确，耗时与视频长度成正比。
    Precise,
}

impl CutMode {
    pub fn label(&self) -> &'static str {
        match self {
            CutMode::Fast => "快速（流复制，无损）",
            CutMode::Precise => "精确（重新编码，帧精确）",
        }
    }

    /// `false` 表示输出起点可能比目标早一点点。
    pub fn is_frame_accurate(&self) -> bool {
        matches!(self, CutMode::Precise)
    }
}

/// 一次切割请求。
#[derive(Debug, Clone)]
pub struct CutRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    /// 要保留的区间，必须按时间升序且互不重叠（由 [`crate::model::keep_segments`] 保证）
    pub keep: Vec<Segment>,
    pub mode: CutMode,
    /// 输出文件已存在时是否覆盖
    pub overwrite: bool,
}

/// 一次切割的结果。
#[derive(Debug, Clone)]
pub struct CutOutcome {
    pub output: PathBuf,
    /// 输出文件实际包含的时长（按计划值累加，不是去 probe 出来的）
    pub kept_secs: f64,
    /// 拼了几段
    pub segments: usize,
    /// 是否走了 concat 拼接这条慢路径
    pub used_concat: bool,
}

/// 切割过程中的回调。
///
/// 刻意做成**一个** trait 而不是「日志闭包 + 进度闭包」两个参数：
/// 调用方（[`crate::pipeline::cut_tasks`]）需要在同一份状态上同时响应这两类事件，
/// 如果传两个 `&mut` 闭包，它们会同时可变借用那块状态 —— 这过不了借用检查
/// （E0524：`two closures require unique access`）。合成一个对象后，
/// 「谁在什么时候能碰 sink」退化成一条清晰的借用链，调用方也不再需要
/// `RefCell` 之类绕路的写法。
///
/// 两个方法都不会被并发调用（`cut` 是在调用线程上同步跑完的），
/// 所以实现方不需要考虑线程安全。
pub trait CutObserver {
    /// ffmpeg 的一行可读输出（stderr 的 info 级日志）。
    fn line(&mut self, line: &str);
    /// 当前进度，0.0 ~ 1.0。
    fn progress(&mut self, fraction: f64);
}

/// 什么都不做的观察者，给「只关心成功与否」的调用方用。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl CutObserver for NoopObserver {
    fn line(&mut self, _line: &str) {}
    fn progress(&mut self, _fraction: f64) {}
}

/// ffmpeg 切割器。
#[derive(Debug, Clone)]
pub struct Cutter {
    pub ffmpeg: PathBuf,
}

impl Cutter {
    pub fn new(ffmpeg: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
        }
    }

    /// 执行切割。
    ///
    /// - `observer`：接收 ffmpeg 的可读输出（GUI 里折叠展示的就是它）与 0.0~1.0 的进度
    pub fn cut(
        &self,
        req: &CutRequest,
        cancel: Option<&CancelFlag>,
        observer: &mut dyn CutObserver,
    ) -> Result<CutOutcome> {
        // ---- 前置校验。这些错误必须在动 ffmpeg 之前拦住 ----
        if req.keep.is_empty() {
            return Err(CoreError::NothingToKeep(req.input.clone()));
        }
        // 覆盖源文件是不可逆的数据损失，不能只是「警告一下」
        if same_file(&req.input, &req.output) {
            return Err(CoreError::OutputEqualsInput(req.input.clone()));
        }
        if req.output.exists() && !req.overwrite {
            return Err(CoreError::OutputExists(req.output.clone()));
        }
        if let Some(parent) = req.output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let total_keep: f64 = req.keep.iter().map(|s| s.duration()).sum();
        if total_keep <= 0.0 {
            return Err(CoreError::NothingToKeep(req.input.clone()));
        }

        if req.keep.len() == 1 {
            self.cut_single(req, total_keep, cancel, observer)?;
            Ok(CutOutcome {
                output: req.output.clone(),
                kept_secs: total_keep,
                segments: 1,
                used_concat: false,
            })
        } else {
            let segments = self.cut_multi(req, total_keep, cancel, observer)?;
            Ok(CutOutcome {
                output: req.output.clone(),
                kept_secs: total_keep,
                segments,
                used_concat: true,
            })
        }
    }

    /// 单段：一条 ffmpeg 命令搞定。
    fn cut_single(
        &self,
        req: &CutRequest,
        total_keep: f64,
        cancel: Option<&CancelFlag>,
        observer: &mut dyn CutObserver,
    ) -> Result<()> {
        let seg = req.keep[0];
        let mut cmd = self.build_cut_command(&req.input, &req.output, seg, req.mode, req.overwrite);

        let mut handle_line = |stream: Stream, line: &str| match stream {
            // `-progress pipe:1` 的输出**全部**在 stdout 上，而且是一堆
            // `key=value`：frame / fps / bitrate / speed / out_time / progress…
            // 只有 out_time_* 有用，其余既不解析也不当日志转发，
            // 否则每半秒就往日志里塞七八行噪音。
            Stream::Stdout => {
                if let Some(secs) = parse_progress_line(line) {
                    observer.progress((secs / total_keep).clamp(0.0, 1.0));
                }
            }
            // 人看的日志都在 stderr 上
            Stream::Stderr => {
                let text = line.trim();
                if !text.is_empty() {
                    observer.line(text);
                }
            }
        };

        let res = run_streaming(&mut cmd, cancel.cloned(), &mut handle_line)?;
        if res.cancelled {
            cleanup_partial(&req.output);
            return Err(CoreError::Cancelled);
        }
        if let Err(e) = res.into_error(&format!("ffmpeg 切割 {}", req.input.display())) {
            cleanup_partial(&req.output);
            return Err(e);
        }
        observer.progress(1.0);
        Ok(())
    }

    /// 多段：逐段落临时文件，再用 concat 拼。返回段数。
    fn cut_multi(
        &self,
        req: &CutRequest,
        total_keep: f64,
        cancel: Option<&CancelFlag>,
        observer: &mut dyn CutObserver,
    ) -> Result<usize> {
        let temp_dir = make_temp_dir(&req.output)?;
        // 不管中途出什么岔子都要清掉临时目录，所以用闭包包住主体再统一清理
        let result = self.cut_multi_inner(req, total_keep, cancel, observer, &temp_dir);
        if let Err(e) = std::fs::remove_dir_all(&temp_dir) {
            tracing::warn!(dir = %temp_dir.display(), error = %e, "临时目录清理失败");
        }
        result
    }

    fn cut_multi_inner(
        &self,
        req: &CutRequest,
        total_keep: f64,
        cancel: Option<&CancelFlag>,
        observer: &mut dyn CutObserver,
        temp_dir: &Path,
    ) -> Result<usize> {
        let ext = req
            .output
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mkv".to_string());

        let mut parts: Vec<PathBuf> = Vec::with_capacity(req.keep.len());
        let mut done_secs = 0.0_f64;

        for (i, seg) in req.keep.iter().enumerate() {
            let part = temp_dir.join(format!("part_{i:03}.{ext}"));
            observer.line(&format!(
                "—— 第 {}/{} 段：{} → {}",
                i + 1,
                req.keep.len(),
                format_timestamp(seg.start),
                format_timestamp(seg.end)
            ));

            let mut cmd = self.build_cut_command(&req.input, &part, *seg, req.mode, true);
            let seg_dur = seg.duration();
            let base = done_secs;
            let mut handle_line = |stream: Stream, line: &str| match stream {
                Stream::Stdout => {
                    if let Some(secs) = parse_progress_line(line) {
                        observer.progress(((base + secs) / total_keep).clamp(0.0, 1.0));
                    }
                }
                Stream::Stderr => {
                    let text = line.trim();
                    if !text.is_empty() {
                        observer.line(text);
                    }
                }
            };

            let res = run_streaming(&mut cmd, cancel.cloned(), &mut handle_line)?;
            if res.cancelled {
                return Err(CoreError::Cancelled);
            }
            res.into_error(&format!("ffmpeg 切割第 {} 段", i + 1))?;
            done_secs += seg_dur;
            parts.push(part);
        }

        // 写 concat 清单
        let list_path = temp_dir.join("concat.txt");
        let mut list = String::new();
        for p in &parts {
            list.push_str(&concat_list_line(p));
            list.push('\n');
        }
        std::fs::write(&list_path, list)?;

        observer.line("—— 拼接分段");
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", "info"])
            .args(["-progress", "pipe:1"])
            .arg("-f")
            .arg("concat")
            // -safe 0：允许清单里出现绝对路径。不加这个，带盘符的 Windows 路径会被拒
            .args(["-safe", "0"])
            .arg("-i")
            .arg(&list_path)
            .args(["-c", "copy", "-avoid_negative_ts", "make_zero"])
            .arg("-y")
            .arg(&req.output);
        no_window(&mut cmd);

        let mut handle_line = |stream: Stream, line: &str| match stream {
            Stream::Stdout => {
                if let Some(secs) = parse_progress_line(line) {
                    observer.progress(((done_secs + secs) / total_keep).clamp(0.0, 1.0));
                }
            }
            Stream::Stderr => {
                let text = line.trim();
                if !text.is_empty() {
                    observer.line(text);
                }
            }
        };
        let res = run_streaming(&mut cmd, cancel.cloned(), &mut handle_line)?;
        if res.cancelled {
            cleanup_partial(&req.output);
            return Err(CoreError::Cancelled);
        }
        if let Err(e) = res.into_error("ffmpeg 拼接分段") {
            cleanup_partial(&req.output);
            return Err(e);
        }

        observer.progress(1.0);
        Ok(parts.len())
    }

    /// 拼出一条切割命令。
    ///
    /// 两种模式的差别只有「输出编码参数」和「要不要 `-map 0`」之外的编码开关，
    /// `-ss` 一律放 `-i` 之前 —— 因为精确模式要重编码，这样反而帧精确（见模块文档）。
    fn build_cut_command(
        &self,
        input: &Path,
        output: &Path,
        seg: Segment,
        mode: CutMode,
        overwrite: bool,
    ) -> Command {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", "info"])
            // 机器可读的进度走 stdout，人看的日志走 stderr，两路不打架
            .args(["-progress", "pipe:1"])
            .arg("-ss")
            .arg(format!("{:.3}", seg.start))
            .arg("-i")
            .arg(input)
            .arg("-t")
            .arg(format!("{:.3}", seg.duration()))
            // 复制所有流：正片之外的字幕、章节、多音轨都得留下来，
            // 否则「去掉片头片尾」顺带把中文字幕也弄丢了
            .arg("-map")
            .arg("0");

        match mode {
            CutMode::Fast => {
                cmd.args(["-c", "copy"]);
            }
            CutMode::Precise => {
                cmd.args(["-c:v", "libx264"])
                    .args(["-crf", &PRECISE_CRF.to_string()])
                    .args(["-preset", PRECISE_PRESET])
                    .args(["-pix_fmt", "yuv420p"])
                    .args(["-c:a", "aac", "-b:a", "192k"])
                    // 字幕流没法重编码就原样复制，能留就留
                    .args(["-c:s", "copy"]);
            }
        }

        // 流复制时把时间戳归零，否则播放器会看到一段「开头几秒是空白」的片子
        cmd.args(["-avoid_negative_ts", "make_zero"]);
        cmd.arg(if overwrite { "-y" } else { "-n" });
        cmd.arg(output);
        no_window(&mut cmd);
        cmd
    }

    /// 用 ffmpeg 抽一帧出来存成 PNG，供界面做缩略图预览。
    ///
    /// 放在这里而不是单独开模块，是因为它跟切割共用同一份 ffmpeg 定位和
    /// 「不弹黑框」处理。
    pub fn extract_frame(&self, video: &Path, at_secs: f64, out_png: &Path) -> Result<()> {
        if let Some(parent) = out_png.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
            // 关键：抽帧是把 -ss 放在 -i **后面**，从头解码到目标时间点，
            // 这样拿到的确实是该时刻的画面而不是它之前的关键帧
            .arg("-i")
            .arg(video)
            .arg("-ss")
            .arg(format!("{:.3}", at_secs.max(0.0)))
            .args(["-frames:v", "1", "-q:v", "2", "-y"])
            .arg(out_png);
        no_window(&mut cmd);

        let mut sink = |_: Stream, _: &str| {};
        let res = run_streaming(&mut cmd, None, &mut sink)?;
        res.into_error("ffmpeg 抽帧")?;
        if !out_png.is_file() {
            return Err(CoreError::ParseFailed {
                what: "ffmpeg 抽帧",
                detail: "命令成功退出但没有产出图片".to_string(),
            });
        }
        Ok(())
    }
}

/// 解析 `-progress pipe:1` 输出的一行，取出已输出的秒数。
///
/// ffmpeg 会依次写 `out_time_us` / `out_time_ms` / `out_time`，取先出现的那个即可。
/// 注意历史上 `out_time_ms` 实际单位也是微秒（ffmpeg 自己的命名不一致），
/// 所以这里统一按「微秒 / 秒」两种单位各自处理，不假设 ms 就是毫秒。
pub fn parse_progress_line(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    let value = value.trim();
    match key {
        "out_time_us" | "out_time_ms" => value.parse::<f64>().ok().map(|us| us / 1_000_000.0),
        "out_time" => parse_hms_to_secs(value),
        _ => None,
    }
}

/// 把 `00:01:23.456789` 解析成秒。
fn parse_hms_to_secs(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    let total = h * 3600.0 + m * 60.0 + sec;
    if total.is_finite() {
        Some(total)
    } else {
        None
    }
}

/// concat 分离器清单里的一行。
///
/// 两点必须处理：路径里的反斜杠统一换成正斜杠（Windows 路径直接写进去，
/// 某些 ffmpeg 版本会把 `\` 当转义符吃掉），以及单引号要按 `'\''` 转义。
fn concat_list_line(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let escaped = s.replace('\'', "'\\''");
    format!("file '{escaped}'")
}

/// 在输出目录下造一个临时目录。
fn make_temp_dir(output: &Path) -> Result<PathBuf> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // 用 pid + 纳秒时间戳，并发切多集时互不撞车
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = parent.join(format!(".intro-outro-tmp-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 判断两个路径是不是同一个文件。
///
/// 用 canonicalize 而不是字符串比较：`./a.mkv` 与 `a.mkv`、大小写不同的
/// Windows 路径都指向同一个文件，字符串比较拦不住，而这关系到会不会覆盖源文件。
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        // 输出文件还不存在时 canonicalize 会失败，就退回字面比较
        _ => a == b,
    }
}

/// 失败后清掉可能残留的半成品输出。
///
/// 失败时 ffmpeg 可能已经建了文件但只写了一半；留着它，用户下次跑会因为
/// 「文件已存在」被拦下来，或者更糟 —— 以为这就是成品。
fn cleanup_partial(path: &Path) {
    if path.is_file() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_progress_lines() {
        assert_eq!(parse_progress_line("out_time_us=1500000"), Some(1.5));
        // ffmpeg 的 out_time_ms 历史上记的就是微秒，别按毫秒算
        assert_eq!(parse_progress_line("out_time_ms=2000000"), Some(2.0));
        assert_eq!(parse_progress_line("out_time=00:01:30.500000"), Some(90.5));
        assert_eq!(
            parse_progress_line("out_time=01:00:00.000000"),
            Some(3600.0)
        );
    }

    #[test]
    fn ignores_non_progress_lines() {
        assert_eq!(parse_progress_line("frame=123"), None);
        assert_eq!(parse_progress_line("progress=continue"), None);
        assert_eq!(parse_progress_line("bitrate=N/A"), None);
        assert_eq!(parse_progress_line("没有等号的一行"), None);
        assert_eq!(parse_progress_line("out_time=N/A"), None);
    }

    #[test]
    fn concat_list_normalises_windows_paths_and_escapes_quotes() {
        let line = concat_list_line(Path::new(r"C:\TV\Show S01E01\part_000.mkv"));
        assert_eq!(line, "file 'C:/TV/Show S01E01/part_000.mkv'");

        let line = concat_list_line(Path::new("/tv/it's here.mkv"));
        assert_eq!(line, r"file '/tv/it'\''s here.mkv'");
    }

    #[test]
    fn detects_same_file_by_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.mkv");
        std::fs::write(&f, b"x").unwrap();
        assert!(same_file(&f, &f));
        // 同一目录下的不同名字不算同一个文件
        assert!(!same_file(&f, &dir.path().join("b.mkv")));
        // 输出文件不存在时走字面比较，也不能误判成同一个
        assert!(!same_file(&f, &dir.path().join("b.mkv")));
    }
}
