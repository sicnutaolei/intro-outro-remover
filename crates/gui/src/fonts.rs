//! 中文字体加载。
//!
//! # 为什么这件事必须显式做
//!
//! egui 内置的字体只覆盖拉丁字母，**不含任何 CJK 字形**。不去加载一份中文字体，
//! 界面上所有中文都会渲染成 `□□□`（tofu 方块）—— 而这个问题在任何静态检查、
//! 单元测试、`cargo clippy` 里都**看不出来**：代码完全正确，运行时才现原形。
//!
//! 解决办法是从系统字体目录里读一份中文字体，塞进 egui 的字体表，并且
//! **插到最前面**：放在后面的话，拉丁字形仍会优先命中 egui 自带字体，
//! 中文和英文混排时字重和字宽会明显不搭。
//!
//! 只读系统里已有字体，不内嵌字体文件 —— 内嵌一份思源黑体要多出 10 MB 以上，
//! 对一个工具类应用不划算。

use std::sync::Arc;

/// 各平台常见的中文字体，按「UI 显示效果」排序。
///
/// Windows 上首选微软雅黑（`msyh.ttc`）：它是系统 UI 字体，字形和字重最贴合
/// 界面文字。等线（`Deng.ttf`）作次选。黑体（`simhei.ttf`）是纯 TTF，
/// 万一 TTC 字体集合加载有问题时它是可靠的兜底。
const CANDIDATES: &[&str] = &[
    // Windows
    r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑
    r"C:\Windows\Fonts\Deng.ttf",   // 等线
    r"C:\Windows\Fonts\simhei.ttf", // 黑体
    r"C:\Windows\Fonts\simsun.ttc", // 宋体
    // macOS
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    // Linux
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/arphic/uming.ttc",
];

/// 加载结果。
pub struct FontSetup {
    /// 实际用上的字体文件路径；`None` 表示没找到任何中文字体。
    pub loaded_from: Option<String>,
    /// 找过但没读成的路径。
    ///
    /// 记下具体路径而不是只记个数：用户看到「找不到字体」时第一反应是
    /// 「我明明有啊」，这时能直接告诉他程序究竟去哪些位置找过，
    /// 比一句「未找到」有用得多。
    pub attempted: Vec<String>,
}

/// 把系统里的中文字体装进 egui。
///
/// 返回的 `loaded_from` 为 `None` 时，调用方应当在界面上明确提示用户「未找到
/// 中文字体，界面可能显示为方块」—— 静默失败会让用户以为是程序坏了。
pub fn install_cjk_fonts(ctx: &egui::Context) -> FontSetup {
    let mut attempted: Vec<String> = Vec::new();

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            attempted.push((*path).to_owned());
            continue;
        };
        // 空文件或异常小的文件说明读到了占位文件，换下一个
        if bytes.len() < 1024 {
            tracing::debug!(path, bytes = bytes.len(), "字体文件过小，跳过");
            attempted.push((*path).to_owned());
            continue;
        }

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            // egui 从 0.29 起用 Arc<FontData> 共享字体数据，避免每帧克隆
            Arc::new(egui::FontData::from_owned(bytes)),
        );

        // 插到最前：中英文混排时优先用同一份字体渲染，字重才一致
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        // 等宽族里追加而不是插到最前：代码/路径之类的内容保持等宽更好读，
        // 只有等宽字体没有的 CJK 字形才落到这里
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());

        ctx.set_fonts(fonts);
        tracing::info!(path, "已加载中文字体");
        return FontSetup {
            loaded_from: Some((*path).to_owned()),
            attempted,
        };
    }

    tracing::warn!("没有找到任何可用的中文字体，界面上的中文可能显示为方块");
    FontSetup {
        loaded_from: None,
        attempted,
    }
}
