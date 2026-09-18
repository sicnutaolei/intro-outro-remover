//! 子进程执行工具。
//!
//! 这层存在的唯一理由是：GUI 要在**长时间跑命令行工具的同时保持界面可交互**。
//! 直接 `Command::output()` 会阻塞到进程结束，期间进度条不动、取消按钮没反应 ——
//! 对动辄几分钟的 audio 指纹计算来说不可接受。
//!
//! 所以这里做三件 `output()` 不做的事：
//! 1. stdout / stderr 分别用独立线程按行读取并回传，主线程负责回调，
//!    这样既不会因为管道缓冲区写满而把子进程卡死，也不会丢输出；
//! 2. 一个看门狗线程盯着取消标志去 `kill` 子进程 —— 靠「读输出时顺便检查」
//!    是不够的，像 needle 分析阶段那样长时间一个字都不输出的进程会完全不可取消；
//! 3. 输出按行留存尾部若干行，命令失败时可以直接把它塞进错误信息里给用户看。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::error::{CoreError, Result};

/// 默认保留的输出尾部行数。
///
/// 出错时用户需要看到的是「最后几行到底报了什么」，而不是从头开始的两千行
/// 解码日志。这个值同时也是总捕获上限，避免坏输入把内存吃光。
const DEFAULT_TAIL_LINES: usize = 200;

/// 输出来自哪一路。
///
/// 必须区分：ffmpeg 的 `-progress pipe:1` 进度行只在 stdout 上，
/// 而它真正的人类可读报错在 stderr 上。混在一起就没法既解析进度又保留报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    pub fn label(&self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// 一次子进程调用的结果。
#[derive(Debug)]
pub struct ExecResult {
    pub status: ExitStatus,
    /// 被取消标志截断
    pub cancelled: bool,
    /// 输出尾部若干行（stdout 与 stderr 合并，按到达顺序）
    pub tail: Vec<String>,
    /// 仅 stderr 的尾部。报错时优先展示这个，因为工具的可读错误都在 stderr
    pub stderr_tail: Vec<String>,
}

impl ExecResult {
    pub fn success(&self) -> bool {
        self.status.success() && !self.cancelled
    }

    /// 退出码。被信号杀掉时 `None`。
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// 尾部若干行拼成一段文本，用于拼错误信息。
    pub fn tail_text(&self) -> String {
        let src = if self.stderr_tail.is_empty() {
            &self.tail
        } else {
            &self.stderr_tail
        };
        src.join("\n")
    }

    /// 把非零退出码转成错误；成功或已取消时返回 `Ok`。
    ///
    /// 取消是**预期内的正常路径**（用户点了一下按钮），所以它不产生
    /// `CommandFailed`，由调用方自己判断 `cancelled` 后返回 `Cancelled`。
    pub fn into_error(self, cmd_display: &str) -> Result<Self> {
        if self.cancelled || self.status.success() {
            return Ok(self);
        }
        // 退出码要拼成人话。被信号杀掉时 code() 是 None，那种情况说「无退出码」
        // 比打印 None 有用得多。
        let code = match self.code() {
            Some(c) => format!("退出码 {c}"),
            None => "进程被强制终止，无退出码".to_string(),
        };
        Err(CoreError::CommandFailed {
            cmd: cmd_display.to_string(),
            code,
            stderr: self.tail_text(),
        })
    }
}

/// 给命令加上「不弹控制台窗口」标志。
///
/// 对 GUI 应用来说这是必须的：Windows 上每 spawn 一个控制台程序都会闪一个黑框，
/// 批量切 20 集会闪 20 次。这个标志对控制台程序的行为本身没有影响。
pub fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// 一次性拿全输出的结果，用于短命令（ffprobe 探测、版本查询）。
#[derive(Debug)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// 跑一条短命令并把 stdout / stderr 全部读进内存。
///
/// 只适合输出量可控的命令 —— ffprobe 的 JSON、`ffmpeg -version` 这类。
/// 长时间跑的 ffmpeg 切割必须走 [`run_streaming`]，否则管道缓冲区写满会死锁。
pub fn capture(cmd: &mut Command) -> Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(cmd);
    let out = cmd.output()?;
    Ok(Captured {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// 运行一个子进程，边跑边把输出按行回调给调用方，返回时带上退出状态。
///
/// `on_line` 在**主线程**上被调用，所以不需要 `Send`，可以放心捕获 GUI 状态。
pub fn run_streaming(
    cmd: &mut Command,
    cancel: Option<Arc<AtomicBool>>,
    on_line: &mut dyn FnMut(Stream, &str),
) -> Result<ExecResult> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    no_window(cmd);

    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout 已被 piped() 设置为管道，take 不可能失败");
    let stderr = child
        .stderr
        .take()
        .expect("stderr 已被 piped() 设置为管道，take 不可能失败");

    // 两路输出汇聚到一个 channel，主线程单点消费，避免并发调用 on_line
    let (tx, rx) = mpsc::channel::<(Stream, String)>();
    let readers = [
        spawn_reader(stdout, Stream::Stdout, tx.clone()),
        spawn_reader(stderr, Stream::Stderr, tx),
    ];

    // 看门狗：定时检查取消标志并 kill 子进程。
    // 用 Mutex<Child> 共享是因为 kill 需要 &mut Child。
    let child = Arc::new(Mutex::new(child));
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog = cancel.as_ref().map(|flag| {
        let child = Arc::clone(&child);
        let finished = Arc::clone(&finished);
        let flag = Arc::clone(flag);
        thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if flag.load(Ordering::Relaxed) {
                    if let Ok(mut c) = child.lock() {
                        let _ = c.kill();
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(80));
            }
        })
    });

    let mut tail: Vec<String> = Vec::new();
    let mut stderr_tail: Vec<String> = Vec::new();
    // rx 在所有发送端 drop 后自动结束 —— 发送端是那两个读线程，
    // 它们会在管道 EOF（也就是子进程退出）时结束。
    for (stream, line) in rx {
        if stream == Stream::Stderr {
            push_capped(&mut stderr_tail, &line);
        }
        push_capped(&mut tail, &line);
        on_line(stream, &line);
    }

    finished.store(true, Ordering::Relaxed);
    if let Some(h) = watchdog {
        let _ = h.join();
    }
    for r in readers {
        let _ = r.join();
    }

    // 收割退出状态。
    //
    // 这里刻意用「短暂持锁 + try_wait」而不是「持锁 wait()」：如果子进程关了
    // 管道但还没退出，持锁 wait() 会把看门狗挡在锁外面，取消就再也生效不了。
    let status = loop {
        {
            let mut c = child.lock().expect("子进程互斥锁不该中毒");
            if let Some(s) = c.try_wait()? {
                break s;
            }
        }
        thread::sleep(Duration::from_millis(20));
    };

    let cancelled = cancel
        .as_ref()
        .map(|f| f.load(Ordering::Relaxed))
        .unwrap_or(false);

    Ok(ExecResult {
        status,
        cancelled,
        tail,
        stderr_tail,
    })
}

/// 起一个线程把管道按行读出来送到 channel。
fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    stream: Stream,
    tx: mpsc::Sender<(Stream, String)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            // 行里有非法 UTF-8 时不用直接放弃这一行 —— 用有损转换，
            // 否则 Windows 上某些 ffmpeg 的中文路径报错会被整行吞掉。
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    // 读错误意味着管道坏了，继续读没意义
                    let _ = tx.send((stream, format!("<读取输出失败：{e}>")));
                    break;
                }
            };
            if tx.send((stream, line)).is_err() {
                break;
            }
        }
    })
}

/// 往有上限的缓冲里追加一行，超长时丢最旧的。
fn push_capped(buf: &mut Vec<String>, line: &str) {
    if buf.len() >= DEFAULT_TAIL_LINES {
        buf.remove(0);
    }
    buf.push(line.to_string());
}

/// 在给定目录或 PATH 中查找可执行文件。
///
/// 顺序是「显式指定的目录」优先于 PATH —— 用户手动指了 `C:\tools\ffmpeg\bin`
/// 就是明确表达「用这个」，不能被 PATH 里另一个旧版本盖掉。
pub fn find_executable(name: &str, explicit_dir: Option<&Path>) -> Option<PathBuf> {
    let file_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);

    if let Some(dir) = explicit_dir {
        let candidate = dir.join(&file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // 用户可能指到的是 ffmpeg 的根目录而不是 bin 子目录，帮他一程
        let nested = dir.join("bin").join(&file_name);
        if nested.is_file() {
            return Some(nested);
        }
    }

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
