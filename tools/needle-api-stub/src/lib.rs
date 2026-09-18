//! needle-rs 的 API stub，只为类型检查 `--features needle-lib` 那条路径。
//! 用法见 `tools/check-needle-lib.sh`，设计理由见同目录 `Cargo.toml` 的注释。
//!
//! 签名逐字抄自上游（`1e6f92e14aebb797cc2a2a3014c8efcb3cccad9c`）：
//! - `needle/src/audio/analyzer.rs`
//! - `needle/src/audio/comparator.rs`
//! - `needle/src/audio/mod.rs`
//! - `needle/src/lib.rs`（`Error` / `Result`）
//!
//! 方法体一律是占位实现，且这些方法**不会被调用** ——
//! 我们要的只是「编译器看一遍调用点，确认签名对得上」。

// stub 天生是「有字段没人读、有方法没人调」的架子。不关掉死代码检查的话，
// 它自己会刷出几条警告，把我们代码里**真正**的问题淹掉。
#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

/// 上游 `needle/src/lib.rs:118`。
#[derive(Debug)]
pub enum Error {
    /// 占位变体。真实上游有若干变体，我们只需要它实现 `Display`。
    Stub,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "needle stub error")
    }
}

impl std::error::Error for Error {}

/// 上游 `needle/src/lib.rs:152`：`pub type Result<T> = std::result::Result<T, Error>;`
pub type Result<T> = std::result::Result<T, Error>;

pub mod audio {
    use std::path::Path;
    use std::time::Duration;

    use super::{Error, Result};

    /// 上游 `needle/src/audio/mod.rs:39`：`pub const DEFAULT_HASH_DURATION: f32 = 0.3;`
    pub const DEFAULT_HASH_DURATION: f32 = 0.3;

    /// 上游 `needle/src/audio/data.rs:75`。内部字段与我们要验证的内容无关，
    /// 但要派生出上游那份 `Debug`/`Clone`，以免将来调用点需要它们时才发现。
    #[derive(Debug, Clone, Default)]
    pub struct FrameHashes {
        _private: (),
    }

    /// 上游 `needle/src/audio/comparator.rs:66`。
    ///
    /// **字段必须保持私有** —— 这正是 `needle_lib.rs` 模块文档里那条结论
    /// （「结果只能靠 skip file 传出来」）的依据。如果将来有人误以为可以从
    /// 这里读结果，这个 stub 会以同样的方式拒绝他。
    #[derive(Debug, Default)]
    pub struct SearchResult {
        opening: Option<(Duration, Duration)>,
        ending: Option<(Duration, Duration)>,
    }

    /// 上游 `needle/src/audio/analyzer.rs:86`。
    #[derive(Debug, Clone)]
    pub struct Analyzer<P: AsRef<Path>> {
        videos: Vec<P>,
        include_endings: bool,
        threaded_decoding: bool,
        force: bool,
    }

    /// 上游 `needle/src/audio/analyzer.rs:95`。
    impl<P: AsRef<Path>> Default for Analyzer<P> {
        fn default() -> Self {
            Self {
                videos: Vec::new(),
                include_endings: false,
                threaded_decoding: false,
                force: false,
            }
        }
    }

    /// 上游 `needle/src/audio/analyzer.rs:108`。
    impl<P: AsRef<Path>> Analyzer<P> {
        /// `analyzer.rs:110`
        pub fn from_files(
            videos: impl Into<Vec<P>>,
            threaded_decoding: bool,
            force: bool,
        ) -> Self {
            Self {
                videos: videos.into(),
                include_endings: false,
                threaded_decoding,
                force,
            }
        }

        /// `analyzer.rs:136`
        pub fn with_include_endings(mut self, include_endings: bool) -> Self {
            self.include_endings = include_endings;
            self
        }

        /// `analyzer.rs:142`
        pub fn with_threaded_decoding(mut self, threaded_decoding: bool) -> Self {
            self.threaded_decoding = threaded_decoding;
            self
        }
    }

    /// 上游 `needle/src/audio/analyzer.rs:423` —— 注意 `+ Sync` 这个约束，
    /// 抄漏了就会把「类型不满足 Sync」这类错误放进盲区。
    impl<P: AsRef<Path> + Sync> Analyzer<P> {
        /// `analyzer.rs:425`：`pub fn run(&self, hash_duration: Duration, persist: bool, threading: bool) -> Result<Vec<FrameHashes>>`
        ///
        /// 首参是 `Duration` 而不是秒数 —— 上游 `lib.rs` 的文档示例写成
        /// `analyzer.run(1.0, 3.0, false, true)`（4 个参数），那是**过期的**；
        /// 因为上游设了 `doctest = false`，这段示例从来没被编译验证过。
        pub fn run(
            &self,
            hash_duration: Duration,
            persist: bool,
            threading: bool,
        ) -> Result<Vec<FrameHashes>> {
            let _ = (hash_duration, persist, threading);
            Ok(Vec::new())
        }
    }

    /// 上游 `needle/src/audio/comparator.rs:74`。
    #[derive(Debug, Clone)]
    pub struct Comparator<P: AsRef<Path>> {
        videos: Vec<P>,
        include_endings: bool,
        hash_match_threshold: u32,
        min_opening_duration: Duration,
        min_ending_duration: Duration,
        time_padding: Duration,
    }

    /// 上游 `needle/src/audio/comparator.rs:83`。
    impl<P: AsRef<Path>> Default for Comparator<P> {
        fn default() -> Self {
            Self {
                videos: Vec::new(),
                include_endings: false,
                hash_match_threshold: 10,
                min_opening_duration: Duration::from_secs(0),
                min_ending_duration: Duration::from_secs(0),
                time_padding: Duration::ZERO,
            }
        }
    }

    /// 上游 `needle/src/audio/comparator.rs:96`。
    impl<P: AsRef<Path>> From<Analyzer<P>> for Comparator<P> {
        fn from(analyzer: Analyzer<P>) -> Self {
            Self {
                videos: analyzer.videos,
                ..Self::default()
            }
        }
    }

    /// 上游 `needle/src/audio/comparator.rs:106`。
    impl<P: AsRef<Path>> Comparator<P> {
        /// `comparator.rs:120`
        pub fn with_include_endings(mut self, include_endings: bool) -> Self {
            self.include_endings = include_endings;
            self
        }

        /// `comparator.rs:126`：注意是 `u32`
        pub fn with_hash_match_threshold(mut self, hash_match_threshold: u32) -> Self {
            self.hash_match_threshold = hash_match_threshold;
            self
        }

        /// `comparator.rs:132`
        pub fn with_min_opening_duration(mut self, min_opening_duration: Duration) -> Self {
            self.min_opening_duration = min_opening_duration;
            self
        }

        /// `comparator.rs:138`
        pub fn with_min_ending_duration(mut self, min_ending_duration: Duration) -> Self {
            self.min_ending_duration = min_ending_duration;
            self
        }

        /// `comparator.rs:144`
        pub fn with_time_padding(mut self, time_padding: Duration) -> Self {
            self.time_padding = time_padding;
            self
        }
    }

    /// 上游 `needle/src/audio/comparator.rs:518`。
    impl<P: AsRef<Path> + Sync> Comparator<P> {
        /// `comparator.rs:524`
        pub fn run_with_frame_hashes(
            &self,
            frame_hashes: Vec<FrameHashes>,
            display: bool,
            use_skip_files: bool,
            write_skip_files: bool,
            threading: bool,
        ) -> Result<Vec<SearchResult>> {
            let _ = (
                frame_hashes,
                display,
                use_skip_files,
                write_skip_files,
                threading,
            );
            Ok(Vec::new())
        }
    }
}
