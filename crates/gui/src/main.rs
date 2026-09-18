//! 程序入口。
//!
//! 职责只有三件事：初始化日志、配置窗口、把 [`app::App`] 交给 eframe 跑起来。
//! 所有界面逻辑在 `app.rs`，所有线程与消息在 `worker.rs`。
//!
//! 日志默认写到 stderr。想看得更细可以设环境变量：
//!
//! ```text
//! set RUST_LOG=intro_outro_core=debug,intro_outro_gui=debug
//! ```

mod app;
mod fonts;
mod worker;

use eframe::egui;

/// 窗口标题。同时用作 eframe 的应用 id。
const APP_TITLE: &str = "剧集片头片尾批量去除工具";

fn main() -> eframe::Result<()> {
    init_tracing();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // 表格有 7 列、还要并排放右侧预览面板，窗口太小会很难看
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([1000.0, 660.0])
            .with_title(APP_TITLE),
        ..Default::default()
    };

    let result = eframe::run_native(
        APP_TITLE,
        options,
        // eframe 0.26 起创建闭包要返回 Result，方便在初始化阶段就报告错误
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    );

    if let Err(e) = &result {
        // 窗口起不来时要给人一句能行动的话，而不是一串 DRM/GL 内部错误
        eprintln!("启动界面失败：{e}");
        eprintln!("常见原因：没有可用的图形环境（远程 SSH 会话、无显卡驱动的容器）。");
    }
    result
}

/// 初始化日志。
///
/// 默认 `info` 级别 —— `debug` 会把 needle 和 ffmpeg 的每一行都刷出来，
/// 终端上根本没法看。需要排查时用 `RUST_LOG` 环境变量临时调高。
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // 不打模块路径：这个项目模块不多，打出来纯属噪音
        .with_target(false)
        .init();
}
