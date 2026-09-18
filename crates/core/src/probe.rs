//! 定位并调用 `ffmpeg` / `ffprobe`。
//!
//! 切割引擎选择调用系统安装的 ffmpeg CLI，而不是把 ffmpeg 静态链进二进制。
//! 代价是用户必须自己装 ffmpeg；换来的是构建时间从数小时降到几十秒、
//! 二进制体积从几十 MB 降到几 MB，而且用户能用上自己熟悉的那份 ffmpeg
//! （硬件编码器支持、编解码器覆盖面都跟着他那份走）。
//!
//! 顺带一个容易踩的坑：ffprobe 对**部分损坏或非标准封装**的文件会返回非零
//! 退出码，但 stdout 上仍然是一份完整的 JSON。所以这里不能只看退出码，
//! 必须以「能不能解析出时长」为准。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::exec::{capture, find_executable};

/// 探测到的媒体信息。
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// 时长（秒）
    pub duration_secs: f64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 有没有音轨。needle 靠音频做指纹，没有音轨的文件送进去必定失败，
    /// 所以要提前拦下来给用户一句明确的提示。
    pub has_audio: bool,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

/// ffmpeg / ffprobe 的定位结果。
#[derive(Debug, Clone)]
pub struct FfmpegTools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegTools {
    /// 查找 ffmpeg 与 ffprobe。
    ///
    /// `explicit_dir` 优先于 PATH（见 [`find_executable`]）。两个都找不到时
    /// 返回 [`CoreError::ToolNotFound`]，附带的提示文本是给最终用户看的。
    pub fn discover(explicit_dir: Option<&Path>) -> Result<Self> {
        let ffmpeg = find_executable("ffmpeg", explicit_dir)
            .ok_or(CoreError::ToolNotFound { tool: "ffmpeg" })?;
        let ffprobe = find_executable("ffprobe", explicit_dir)
            .ok_or(CoreError::ToolNotFound { tool: "ffprobe" })?;
        Ok(Self { ffmpeg, ffprobe })
    }

    /// 取 `ffmpeg -version` 的第一行，用于在界面上显示用的是哪个版本。
    pub fn version(&self) -> Result<String> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.arg("-version");
        let out = capture(&mut cmd)?;
        let first = out.stdout.lines().next().unwrap_or("").trim().to_string();
        if first.is_empty() {
            return Err(CoreError::ParseFailed {
                what: "ffmpeg -version",
                detail: out.stderr.trim().to_string(),
            });
        }
        Ok(first)
    }

    /// 用 ffprobe 读一个视频的时长、分辨率和音轨情况。
    pub fn probe(&self, video: &Path) -> Result<MediaInfo> {
        let mut cmd = Command::new(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(video);

        let out = capture(&mut cmd)?;
        // 故意不看 out.status：见模块文档，损坏文件会带非零码但输出完整的 JSON
        let parsed: FfprobeOutput =
            serde_json::from_str(out.stdout.trim()).map_err(|e| CoreError::ParseFailed {
                what: "ffprobe JSON",
                detail: format!("{e}；ffprobe 输出首行：{}", first_line(&out.stdout)),
            })?;

        let duration_secs = parsed
            .format
            .as_ref()
            .and_then(|f| f.duration.as_deref())
            .and_then(parse_f64)
            // 有些封装容器不写 format.duration，退回到视频流自己的时长
            .or_else(|| {
                parsed
                    .streams
                    .iter()
                    .find(|s| s.codec_type.as_deref() == Some("video"))
                    .and_then(|s| s.duration.as_deref())
                    .and_then(parse_f64)
            })
            .ok_or_else(|| CoreError::DurationUnknown(video.to_path_buf()))?;

        if !duration_secs.is_finite() || duration_secs <= 0.0 {
            return Err(CoreError::DurationUnknown(video.to_path_buf()));
        }

        let video_stream = parsed
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("video"));
        let audio_stream = parsed
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("audio"));

        Ok(MediaInfo {
            duration_secs,
            width: video_stream.and_then(|s| s.width),
            height: video_stream.and_then(|s| s.height),
            has_audio: audio_stream.is_some(),
            video_codec: video_stream.and_then(|s| s.codec_name.clone()),
            audio_codec: audio_stream.and_then(|s| s.codec_name.clone()),
        })
    }
}

/// `"1440.123456"` / `"1440"` 都能解析；`"N/A"` 这类垃圾返回 `None`。
fn parse_f64(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// ffprobe JSON 的映射结构
//
// 全部字段都是 Option 或带 #[serde(default)]：ffprobe 的字段集合会随版本和
// 容器类型变化（mkv 与 mp4 输出的 stream 字段就不一样），少一个字段不该让
// 整个探测失败。
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_mp4_probe() {
        let json = r#"{
            "streams": [
                {"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
                {"codec_type":"audio","codec_name":"aac"}
            ],
            "format": {"duration":"1440.123456"}
        }"#;
        let p: FfprobeOutput = serde_json::from_str(json).unwrap();
        assert_eq!(p.streams.len(), 2);
        assert_eq!(
            p.format.as_ref().unwrap().duration.as_deref(),
            Some("1440.123456")
        );
    }

    #[test]
    fn parses_a_mkv_probe_with_missing_fields() {
        // mkv 的 stream 常常没有 duration / width 之类的字段，结构体不能因此炸掉
        let json = r#"{
            "streams": [{"codec_type":"video","codec_name":"hevc"}],
            "format": {"duration":"1500.0","nb_streams":1}
        }"#;
        let p: FfprobeOutput = serde_json::from_str(json).unwrap();
        assert_eq!(p.streams[0].width, None);
        assert_eq!(p.streams[0].codec_name.as_deref(), Some("hevc"));
    }

    #[test]
    fn parses_probe_without_format_section() {
        let json = r#"{"streams":[{"codec_type":"video","duration":"600.5"}]}"#;
        let p: FfprobeOutput = serde_json::from_str(json).unwrap();
        assert!(p.format.is_none());
        assert_eq!(p.streams[0].duration.as_deref(), Some("600.5"));
    }

    #[test]
    fn duration_parser_rejects_non_numeric_junk() {
        assert_eq!(parse_f64("1440.5"), Some(1440.5));
        assert_eq!(parse_f64(" 1440 "), Some(1440.0));
        // ffprobe 真的会写 "N/A" 出来
        assert_eq!(parse_f64("N/A"), None);
        assert_eq!(parse_f64("nan"), None);
        assert_eq!(parse_f64("inf"), None);
    }
}
