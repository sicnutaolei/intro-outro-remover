//! 错误类型。
//!
//! 引擎对外的所有失败都收敛到 [`CoreError`] 一个枚举，GUI 层只要把它
//! `to_string()` 就能给用户一句能看懂的中文说明；需要排查时再看 `source()`。
//!
//! 这里刻意不用 `anyhow`：`core` 是要被 GUI 和未来 CLI 复用的库，库的公开
//! 错误类型必须是具体的、可被调用方 `match` 的。`anyhow` 只出现在 GUI 和
//! 内部临时凑合的地方。

use std::path::PathBuf;

/// 核心引擎的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 外部可执行文件找不到。
    ///
    /// 用 `'static str` 而不是 `String`，因为这里只会是 ffmpeg / ffprobe / needle
    /// 三个编译期就确定的字面量。
    #[error("未找到 {tool}，请先安装它并确保它在 PATH 中，或在本工具里手动指定其所在目录")]
    ToolNotFound { tool: &'static str },

    /// 外部命令跑起来了但退出码非 0。
    ///
    /// 退出码存成 `String` 而不是 `Option<i32>`：`Option<i32>` 没有实现
    /// `Display`，而「被信号杀掉所以没有退出码」这句话必须能拼进错误信息里
    /// 给用户看，不能只打印一个 `None`。
    #[error("{cmd} 执行失败（{code}）")]
    CommandFailed {
        cmd: String,
        code: String,
        /// stderr 的尾部若干行，用来在 GUI 上展开查看
        stderr: String,
    },

    /// 输出文件已存在且没有允许覆盖。
    ///
    /// 单独一个变体是为了给出可操作的建议 —— 用户看到「文件已存在」时
    /// 需要知道去哪打开开关，而不只是一句报错。
    #[error("输出文件已存在：{0}（勾选「覆盖已存在的输出文件」可以覆盖它）")]
    OutputExists(PathBuf),

    /// 外部命令输出解析失败。
    #[error("解析 {what} 的输出失败：{detail}")]
    ParseFailed { what: &'static str, detail: String },

    /// ffprobe 认不出这个文件。
    #[error("无法从视频中读出时长：{0}")]
    DurationUnknown(PathBuf),

    /// needle 是靠音频做指纹比对的，没有音轨就没法分析。
    #[error("文件不含音轨，needle 无法分析：{0}")]
    NoAudioStream(PathBuf),

    /// 跨集比对至少需要两集。
    #[error("只找到 {0} 个视频文件；跨集比对至少需要 2 集才能工作")]
    NotEnoughEpisodes(usize),

    /// 检测跑完了但一个片头片尾都没找到。
    #[error("检测完成，但没有任何剧集找到公共片头或片尾（这些剧集之间可能本来就没有相同片头）")]
    NoDetectionAtAll,

    /// 用户取消了任务。
    #[error("任务已取消")]
    Cancelled,

    /// 切割输入与输出是同一个文件 —— 会把自己的源文件覆盖掉，直接拒绝。
    #[error("输出路径与输入相同，拒绝执行以免覆盖源文件：{0}")]
    OutputEqualsInput(PathBuf),

    /// 切割计划里没有任何要保留的区间。
    #[error("片头片尾覆盖了整段视频，没有可保留的内容：{0}")]
    NothingToKeep(PathBuf),

    /// 底层 IO 错误。
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    /// JSON 解析错误。
    #[error("JSON 解析错误：{0}")]
    Json(#[from] serde_json::Error),
}

/// 本 crate 的统一 `Result` 别名。
pub type Result<T> = std::result::Result<T, CoreError>;
