//! 直接链接 needle-rs 库的检测后端（需 `--features needle-lib`）。
//!
//! # 它换来了什么
//!
//! 只换到一件事，但是件实事：**不需要 `.needle.dat` 帧哈希旁挂文件**。
//!
//! CLI 后端要检测片尾，就非得先落一份带片尾数据的 `.needle.dat` 到用户的剧集
//! 目录里不可（原因见父模块文档里那条上游行为）。而在这里，`FrameHashes` 是
//! 内存里的对象，直接顺给 `Comparator` 就行，磁盘上干干净净。
//!
//! # 它付出了什么
//!
//! 1. **构建代价**：`needle-rs` 依赖 `ffmpeg-next` 7.0，要 FFmpeg 开发库。
//!    Linux/macOS 上装几个 `-dev` 包就行；Windows 上按上游 README 得走
//!    `cargo vcpkg build`，用 vcpkg 从源码编译 FFmpeg，以小时计。
//!    另外它默认 feature 里带 `static-chromaprint`，还要求有 cmake。
//! 2. **取消不灵敏**：`Analyzer::run` / `Comparator::run_with_frame_hashes`
//!    都是同步阻塞调用，中途没有回调也没有取消入口。所以这个后端的「取消」
//!    只能在一个阶段结束后生效，不能像 CLI 后端那样把子进程直接杀掉。
//!    这是**库本身的接口限制**，不是实现偷懒。
//! 3. **拿结果仍然要落盘**：`needle::audio::SearchResult` 的字段是私有的、
//!    没有任何访问器，所以即使结果已经在内存里，我们也读不出来 ——
//!    只能让它写成 skip file 再读回来（见 [`crate::skipfile`] 的文档）。
//!    所以 `.needle.skip.json` 这一类文件在两种后端下都躲不掉。
//!
//! # 结论
//!
//! 除非你真的不能忍受源目录里多一个 76 KB 级的 `.needle.dat`，否则默认的
//! CLI 后端在构建成本上完胜。这个后端保留下来是为了「想跟上游修 bug 时
//! 直接改依赖版本重编」这条路径。

use std::path::PathBuf;
use std::time::Duration;

use needle::audio::{Analyzer, Comparator};

use crate::error::{CoreError, Result};
use crate::model::Detection;

use super::{
    check_cancel, collect_detections, CancelFlag, DetectEvent, DetectOptions, SidecarSnapshot,
};

/// 直接链接 needle-rs 的检测后端。
#[derive(Debug, Clone, Default)]
pub struct LibraryDetector;

impl LibraryDetector {
    pub fn new() -> Self {
        Self
    }

    /// 跑完整套检测，返回与 `files` 按下标一一对应的结果。
    ///
    /// 签名与 [`super::CliDetector::detect`] 完全一致，两者可直接互换。
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

    fn run_pipeline(
        &self,
        files: &[PathBuf],
        opts: &DetectOptions,
        cancel: &CancelFlag,
        emit: &mut dyn FnMut(DetectEvent),
    ) -> Result<Vec<Detection>> {
        check_cancel(cancel)?;
        emit(DetectEvent::Stage(if opts.include_endings {
            "分析音频指纹（含片尾片段）".to_string()
        } else {
            "分析音频指纹".to_string()
        }));
        emit(DetectEvent::Log(
            "库后端直接对全部剧集并行解码，过程中无法回报逐集进度".to_string(),
        ));

        // `from_files(videos, threaded_decoding, force)` —— 第二个参数是「是否让
        // FFmpeg 内部多线程解码」，跟要不要跨文件并行无关（后者由 run 的第三个
        // 参数控制）。
        let analyzer = Analyzer::from_files(files.to_vec(), true, opts.force_reanalyze)
            // 这一行是能检测到片尾的前提。默认值是 false，漏了它就会静默地
            // 只搜片头 —— 而且不会报任何错。
            .with_include_endings(opts.include_endings)
            .with_threaded_decoding(opts.threading);

        // `run(hash_duration, persist, threading)` —— 注意第一个参数是
        // `std::time::Duration`，不是秒数 f64。上游 `lib.rs` 开头的文档注释里
        // 写的是 `analyzer.run(1.0, 3.0, false, true)`（4 个参数），那是**过期的**：
        // 该 crate 的 Cargo.toml 里设了 `doctest = false`，这段示例从来没有被
        // 编译验证过，所以一直没人发现它跟真实签名对不上。
        let frame_hashes = analyzer
            .run(
                Duration::from_secs_f32(needle::audio::DEFAULT_HASH_DURATION),
                // persist = false：不写 .needle.dat。这就是本后端存在的意义
                false,
                opts.threading,
            )
            .map_err(|e| CoreError::ParseFailed {
                what: "needle 音频分析",
                detail: e.to_string(),
            })?;

        check_cancel(cancel)?;
        emit(DetectEvent::Stage(
            "跨集比对，搜索公共片头 / 片尾".to_string(),
        ));

        // 复用上一步的路径列表，省掉再解析一遍参数
        let comparator: Comparator<PathBuf> = analyzer.into();
        let comparator = comparator
            .with_include_endings(opts.include_endings)
            .with_hash_match_threshold(opts.hash_match_threshold)
            .with_min_opening_duration(Duration::from_secs(opts.min_opening_duration_secs as u64))
            .with_min_ending_duration(Duration::from_secs(opts.min_ending_duration_secs as u64))
            .with_time_padding(Duration::from_secs_f32(opts.time_padding_secs as f32));

        comparator
            .run_with_frame_hashes(
                // 文档明确要求：frame_hashes 必须来自**同一份**视频路径列表，
                // 因为它是按下标跟 self.videos 对应的。上面正好满足。
                frame_hashes,
                false, // display：不要它往 stdout 打印
                false, // use_skip_files：不要因为磁盘上有旧结果就跳过这一集
                true,  // write_skip_files：结果唯一的出口，见模块文档
                opts.threading,
            )
            .map_err(|e| CoreError::ParseFailed {
                what: "needle 跨集比对",
                detail: e.to_string(),
            })?;

        check_cancel(cancel)?;
        emit(DetectEvent::Stage("读取检测结果".to_string()));
        collect_detections(files, emit)
    }
}
