//! 读写 needle 的「skip file」。
//!
//! # 为什么需要这个模块
//!
//! needle-rs 作为库使用时，`needle::audio::SearchResult` 的 `opening` / `ending`
//! 字段是**私有的，而且没有任何访问器方法**（没有 `opening()`、没有 `Deref`、
//! 没有 `Into`，只有 `Copy/Clone/Debug/Default`）。它自己的 CLI 能显示结果，
//! 是因为它把 `display = true` 传进去，让库里直接 `println!` 到 stdout ——
//! `main.rs` 里那行 `comparator.run(...)?` 的返回值是被**丢掉**的。
//!
//! 所以无论是「子进程调 needle」还是「直接链接 needle-rs」，要拿到结构化的
//! 时间戳都只有一条路：让 needle 把结果写成 JSON 旁挂文件，再读回来。
//! 这个模块就是那条路的全部实现。
//!
//! # 旁挂文件命名规则
//!
//! needle 用的是 `Path::with_extension`，注意它**替换**原扩展名而不是追加：
//!
//! ```text
//! Show.S01E01.mp4  ->  Show.S01E01.needle.skip.json
//! ```
//!
//! 文件名里本来带点号（`Show.S01E01.1080p.mkv`）不会出问题，因为
//! `with_extension` 替换的是最后一个点号之后的部分。
//!
//! # 一个必须处理的边界
//!
//! `Comparator::create_skip_file` 在「片头片尾都没找到」时会**提前返回，
//! 不写文件**。也就是说「文件不存在」和「检测到空结果」是同一件事 ——
//! 调用方必须把缺文件当成 `Ok(None)`，而不是 IO 错误。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Result;
use crate::model::{Detection, Segment};

/// needle 预计算帧哈希的旁挂文件扩展名。
///
/// # 为什么是个列表而不是一个字符串
///
/// **两个 needle 版本用的名字不一样**：官方 release 的 v0.1.5 在
/// `audio/mod.rs` 里写的是 `"needle.bin"`，main 分支（`lib.rs`）改成了
/// `"needle.dat"`。实测用官方二进制跑完，落在源目录里的是
/// `Show.S01E01.needle.bin`。
///
/// 只认一个名字的话，清理会**静默漏掉**另一半 —— 用户会发现「说好源目录一个字节
/// 都不变，结果多出来一堆文件」，而且这种问题极难排查（清理代码看起来完全正常）。
/// 所以这里把两个都列上，凡是 needle 可能生成的都算它的产物。
pub const FRAME_HASH_EXTS: &[&str] = &["needle.bin", "needle.dat"];

/// needle 检测结果的旁挂文件扩展名（`<视频>.needle.skip.json`）。
///
/// 这个名字在 v0.1.5 和 main 上**是一致的**（两边都叫 `needle.skip.json`），
/// 所以读结果这条路径不受版本差异影响。
pub const SKIP_FILE_EXT: &str = "needle.skip.json";

/// needle 写出来的 skip file 的原始结构。
///
/// 字段名对应 needle `audio/data.rs` 里的 `SkipFile`。`md5` 是视频头部
/// （前若干 MB）的 MD5，用于判断视频文件换过没有；我们在只读场景下不校验它，
/// 所以做成 `Option` 以免上游改结构时整个解析失败。
#[derive(Debug, Clone, Deserialize)]
pub struct RawSkipFile {
    /// `(起始秒, 结束秒)`
    pub opening: Option<(f32, f32)>,
    pub ending: Option<(f32, f32)>,
    #[serde(default)]
    pub md5: Option<String>,
}

impl RawSkipFile {
    /// 转成引擎内部的 [`Detection`]。
    ///
    /// 上游偶发会写出 `end <= start` 的退化区间（比对结果只有一个哈希命中时），
    /// 这里直接丢掉而不是让 0 长度区间流进切割阶段。
    pub fn to_detection(&self) -> Detection {
        Detection {
            opening: to_segment(self.opening),
            ending: to_segment(self.ending),
        }
    }
}

fn to_segment(raw: Option<(f32, f32)>) -> Option<Segment> {
    raw.map(|(s, e)| Segment::new(s as f64, e as f64))
        .filter(|s| s.is_valid())
}

/// 某个视频对应的**所有可能**的帧哈希文件路径（两个版本各一个名字）。
pub fn frame_hash_paths(video: &Path) -> Vec<PathBuf> {
    FRAME_HASH_EXTS
        .iter()
        .map(|ext| video.with_extension(ext))
        .collect()
}

/// 某个视频当前真正存在的帧哈希文件路径。
pub fn existing_frame_hash_paths(video: &Path) -> Vec<PathBuf> {
    frame_hash_paths(video)
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

/// 某个视频对应的检测结果文件路径。
pub fn skip_file_path(video: &Path) -> PathBuf {
    video.with_extension(SKIP_FILE_EXT)
}

/// 某个视频对应的**所有可能**的 needle 旁挂文件路径（哈希 + 检测结果）。
///
/// 清理逻辑用它做「跑之前有哪些」和「跑之后有哪些」的快照对照。
pub fn sidecar_paths(video: &Path) -> Vec<PathBuf> {
    let mut v = frame_hash_paths(video);
    v.push(skip_file_path(video));
    v
}

/// 读取某个视频的检测结果。
///
/// 文件不存在时返回 `Ok(None)`（见模块文档：上游在无结果时根本不写文件）。
pub fn read_detection(video: &Path) -> Result<Option<Detection>> {
    let path = skip_file_path(video);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    // 空文件也见过（进程被中途 kill 掉），当「无结果」处理而不是报错炸掉整轮检测
    if text.trim().is_empty() {
        tracing::warn!(path = %path.display(), "检测结果文件为空，按「未检测到」处理");
        return Ok(None);
    }
    let raw: RawSkipFile = serde_json::from_str(&text)?;
    let det = raw.to_detection();
    Ok(if det.is_empty() { None } else { Some(det) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_paths_replace_the_extension() {
        // 这是必须和上游实现逐字对齐的地方：skip 文件名字写错会静默地
        // 「检测不到任何东西」；哈希文件名写错则会让清理漏掉文件
        let v = PathBuf::from("/videos/Show.S01E01.1080p.mkv");
        assert_eq!(
            skip_file_path(&v),
            PathBuf::from("/videos/Show.S01E01.1080p.needle.skip.json")
        );
        // 两个版本的哈希文件名都要认：v0.1.5 是 needle.bin，main 是 needle.dat
        assert_eq!(
            frame_hash_paths(&v),
            vec![
                PathBuf::from("/videos/Show.S01E01.1080p.needle.bin"),
                PathBuf::from("/videos/Show.S01E01.1080p.needle.dat"),
            ]
        );
        assert_eq!(sidecar_paths(&v).len(), 3);

        // 无扩展名的文件也要能算出名字
        let v = PathBuf::from("/videos/raw");
        assert_eq!(
            skip_file_path(&v),
            PathBuf::from("/videos/raw.needle.skip.json")
        );
    }

    #[test]
    fn parses_a_real_skip_file_payload() {
        // 取自 needle README 里的真实输出
        let raw: RawSkipFile = serde_json::from_str(
            r#"{"opening":null,"ending":[1331.6644,1419.0249],"md5":"14bfa97f"}"#,
        )
        .unwrap();
        let det = raw.to_detection();
        assert!(det.opening.is_none());
        let ending = det.ending.expect("应该解析出片尾");
        assert!((ending.start - 1331.6644).abs() < 1e-3);
        assert!((ending.end - 1419.0249).abs() < 1e-3);
    }

    #[test]
    fn missing_md5_field_still_parses() {
        // 上游未来若去掉 md5 字段，解析不该整体失败
        let raw: RawSkipFile =
            serde_json::from_str(r#"{"opening":[43.1,132.0],"ending":null}"#).unwrap();
        assert_eq!(raw.md5, None);
        assert!(raw.to_detection().opening.is_some());
    }

    #[test]
    fn degenerate_segments_are_dropped() {
        // 只有一个哈希命中时会退化成零长度区间，必须丢掉
        let raw = RawSkipFile {
            opening: Some((100.0, 100.0)),
            ending: Some((200.0, 100.0)),
            md5: None,
        };
        assert!(raw.to_detection().is_empty());
    }
}
