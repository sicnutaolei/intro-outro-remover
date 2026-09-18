//! 端到端集成测试：**真的**调用 ffmpeg 与 needle 跑一遍完整流程。
//!
//! # 为什么要单独放一个文件并且默认 `#[ignore]`
//!
//! 它依赖三件本机不一定有的东西：装了 ffmpeg、装了 needle、以及先用
//! `tools/make_test_clips.sh` 造好的测试素材。把它塞进常规 `cargo test` 会让
//! 没装这些工具的人一直看到红色，久而久之就没人看测试结果了。
//!
//! 所以常规流程只跑纯逻辑单元测试（不需要任何外部工具），这个测试要显式点名：
//!
//! ```bash
//! # 1. 造素材（需要 ffmpeg）
//! bash tools/make_test_clips.sh /path/to/ffmpeg /tmp/ior-clips 4
//!
//! # 2. 指定素材、输出目录与工具位置
//! export IOR_E2E_CLIPS=/tmp/ior-clips
//! export IOR_E2E_DIR=/tmp/ior-out
//! export IOR_FFMPEG_DIR=/path/to/ffmpeg/bin
//! export IOR_NEEDLE_DIR=/path/to/needle
//!
//! # 3. 跑
//! cargo test --test e2e -- --ignored --nocapture
//! ```
//!
//! 断言的是**语义正确性**，不是「没报错」：
//!
//! - 检测出的片头终点要落在素材真实片头长度（25s）附近
//! - 检测出的片尾起点要落在「总时长 − 12s」附近
//! - 结果数量与输入按下标一一对应
//! - 掐头去尾后剩且只剩**一段**
//! - 切割产物的实际时长与计划保留时长吻合
//! - 切割过程中确实收到了进度回调（验证进度解析真的在工作）
//! - **源目录的文件一个字节都没变，且没有残留任何旁挂文件**

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use intro_outro_core::cut::{CutMode, CutOutcome, Cutter};
use intro_outro_core::detect::{new_cancel_flag, CliDetector, DetectOptions};
use intro_outro_core::model::TaskKind;
use intro_outro_core::pipeline::{self, CutSink};
use intro_outro_core::probe::FfmpegTools;

/// 素材里公共片头的长度（秒），跟 make_test_clips.sh 里的 INTRO_SECS 对齐
const EXPECTED_INTRO_SECS: f64 = 25.0;
/// 素材里公共片尾的长度（秒）
const EXPECTED_OUTRO_SECS: f64 = 12.0;
/// 检测结果的容差。needle 给的是「最长公共片段」，与真实边界差一两秒是正常的，
/// 再加上哈希步长（0.3s）和最小片段时长的舍入，给到 4 秒比较合适。
const DETECT_TOLERANCE: f64 = 4.0;
/// needle 报出的边界会**向内**偏大约一个哈希窗口长度。这不是 bug，是它的边界
/// 表示方式决定的：
///
/// * `Comparator` 直接拿哈希点自带的时间戳当边界（`src[src_start_idx].1`）；
/// * 那个时间戳是哈希窗口的**末端**（chromaprint 的延迟指纹器在处理完一整窗
///   音频之后才产出这个点）。
///
/// 于是「匹配到的第一个点」的时间戳天然就是一个窗口的宽度，起点不可能报得更早。
/// 实测：`--hash-duration 3` → 起点 3.0s；`--hash-duration 6` → 起点 6.0s。
/// 而官方 v0.1.5 把 `hash_duration` 的下限钉死在 3 秒
/// （`hash_duration must be greater than 3 seconds`），所以这个偏移在 v0.1.5 上
/// 无法通过调参消除。
///
/// 对使用者的实际影响：片头开头约 3 秒、片尾末尾约 4 秒会留在输出里。
/// 想要更紧的切口就把该行的片头起点改成 0、片尾终点改成视频总长（时间戳可编辑）。
const HASH_QUANTUM_SECS: f64 = 3.0;
/// 素材的关键帧间隔（秒）。`make_test_clips.sh` 用 `-g 25`、25fps 压到了 1 秒，
/// 否则流复制切出来的起点会吸附回去好几秒，断言就只好放宽到没意义的程度。
const KEYFRAME_INTERVAL_SECS: f64 = 1.0;

/// 把切割过程中的回调收集起来，便于断言。
#[derive(Default)]
struct CollectSink {
    lines: Vec<String>,
    progress: Vec<(usize, f64)>,
    finished: Vec<(usize, std::result::Result<PathBuf, String>)>,
}

impl CutSink for CollectSink {
    fn line(&mut self, task_index: usize, line: &str) {
        self.lines.push(format!("[{task_index}] {line}"));
    }
    fn progress(&mut self, task_index: usize, fraction: f64) {
        self.progress.push((task_index, fraction));
    }
    fn finished(&mut self, task_index: usize, outcome: &intro_outro_core::Result<CutOutcome>) {
        let r = match outcome {
            Ok(o) => Ok(o.output.clone()),
            Err(e) => Err(e.to_string()),
        };
        self.finished.push((task_index, r));
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(var).filter(|v| !v.is_empty())?;
    Some(normalise_path(PathBuf::from(raw)))
}

/// 把 MSYS/Git-Bash 风格的路径（`/c/Users/...`）翻成 Windows 风格（`C:/Users/...`）。
///
/// 不翻的话 `PathBuf::from("/c/Users/x")` 在 Windows 上会被解释成
/// 「当前盘根目录下的 `c\Users\x`」，跟死胡同一样找不到文件，
/// 而报错信息只会说「路径不存在」，极难定位。
/// 这样 `bash tools/make_test_clips.sh` 之后直接 `export IOR_E2E_CLIPS=$(pwd)`
/// 也能跑通，不用手动 cygpath。非 Windows 平台原样返回。
fn normalise_path(p: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return p;
    }
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[2] == b'/' && (b[1] as char).is_ascii_alphabetic() {
        let letter = (b[1] as char).to_ascii_uppercase();
        return PathBuf::from(format!("{letter}:{}", &s[2..]));
    }
    p
}

fn require_env(var: &str) -> PathBuf {
    env_path(var).unwrap_or_else(|| {
        panic!("端到端测试需要环境变量 {var}；用法见 crates/core/tests/e2e.rs 的模块文档")
    })
}

/// 数一数目录里 needle 的旁挂文件有几个。
///
/// 两个哈希文件名都要算上：官方 v0.1.5 的二进制写的是 `.needle.bin`，
/// main 分支写的是 `.needle.dat`。只数一个的话，「清理干净了」这个断言会假通过。
fn count_sidecars(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.ends_with(".needle.bin")
                || n.ends_with(".needle.dat")
                || n.ends_with(".needle.skip.json")
        })
        .count()
}

#[test]
#[ignore = "依赖本机的 ffmpeg / needle 与预先生成的素材，需显式 --ignored 运行"]
fn e2e_full_pipeline_on_synthetic_clips() {
    // ---- 环境准备 ----------------------------------------------------
    let clips_dir = require_env("IOR_E2E_CLIPS");
    let out_dir = require_env("IOR_E2E_DIR");
    let tools = FfmpegTools::discover(env_path("IOR_FFMPEG_DIR").as_deref())
        .expect("找不到 ffmpeg / ffprobe");
    let detector =
        CliDetector::discover(env_path("IOR_NEEDLE_DIR").as_deref()).expect("找不到 needle");
    println!("ffmpeg  : {}", tools.ffmpeg.display());
    println!("needle  : {}", detector.needle_exe.display());

    std::fs::create_dir_all(&out_dir).expect("建不出输出目录");
    // 清掉上一轮的输出，否则会撞上「文件已存在」
    for entry in std::fs::read_dir(&out_dir).into_iter().flatten().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }

    // ---- 1. 扫描 ----------------------------------------------------
    let videos = pipeline::scan_videos(std::slice::from_ref(&clips_dir), false).expect("扫描失败");
    assert!(
        videos.len() >= 3,
        "素材不足（只有 {} 个），先用 tools/make_test_clips.sh 造至少 3 集",
        videos.len()
    );
    println!("扫描到 {} 个视频文件", videos.len());

    // 记下源文件的初始状态，最后要验证它们没被动过
    let source_state_before: HashMap<PathBuf, (u64, std::time::SystemTime)> = videos
        .iter()
        .map(|p| {
            let m = std::fs::metadata(p).expect("读不到源文件元数据");
            (p.clone(), (m.len(), m.modified().expect("拿不到修改时间")))
        })
        .collect();
    let sidecars_before = count_sidecars(&clips_dir);
    println!("源目录初始旁挂文件数：{sidecars_before}");

    // ---- 2. 探测媒体信息 --------------------------------------------
    let cancel = new_cancel_flag();
    let files = pipeline::probe_all(&tools, &videos, &cancel, &mut |cur, total, name| {
        if cur == total {
            println!("探测完成 [{cur}/{total}] {name}");
        }
    })
    .expect("探测失败");
    assert_eq!(files.len(), videos.len());

    for f in &files {
        println!(
            "{}  时长 {:.2}s  {}x{}  音轨={}",
            f.name(),
            f.duration_secs.unwrap_or(f64::NAN),
            f.width.unwrap_or(0),
            f.height.unwrap_or(0),
            f.has_audio
        );
        assert!(f.duration_secs.is_some(), "{} 应该能读出时长", f.name());
        assert!(f.has_audio, "{} 应该有音轨", f.name());
    }

    // ---- 3. 检测 ----------------------------------------------------
    let opts = DetectOptions {
        // 素材片尾只有 12 秒，低于 needle 的默认阈值 20 秒 —— 必须显式调小，
        // 这也顺带证明了这个参数确实在起作用
        min_ending_duration_secs: 5,
        // 测试要确定性结果，不复用可能过期的缓存
        force_reanalyze: true,
        ..DetectOptions::default()
    };
    assert!(!opts.keep_sidecars, "默认应该是跑完清理，测试要验证这一点");

    let mut events = Vec::new();
    let detections = detector
        .detect(&videos, &opts, &cancel, &mut |ev| {
            events.push(format!("{ev:?}"))
        })
        .expect("检测失败");
    println!("检测过程共产生 {} 条事件", events.len());
    assert_eq!(
        detections.len(),
        videos.len(),
        "结果数量必须与输入完全一致，否则界面上就会串行"
    );

    for (f, d) in files.iter().zip(detections.iter()) {
        let total = f.duration_secs.expect("前面已经断言过有时长");
        let opening = d
            .opening
            .unwrap_or_else(|| panic!("{} 没检测到片头", f.name()));
        let ending = d.ending.unwrap_or_else(|| {
            panic!(
                "{} 没检测到片尾（上游那个 include_endings 坑没绕过去？）",
                f.name()
            )
        });
        println!(
            "{}  片头 {}  片尾 {}",
            f.name(),
            opening.display(),
            ending.display()
        );

        assert!(
            opening.start <= HASH_QUANTUM_SECS + 1.0,
            "{} 片头起点应贴近 0（最多偏出一个哈希窗口），实际 {:.2}s",
            f.name(),
            opening.start
        );
        assert!(
            (opening.end - EXPECTED_INTRO_SECS).abs() < DETECT_TOLERANCE,
            "{} 片头终点应接近 {EXPECTED_INTRO_SECS}s，实际 {:.2}s",
            f.name(),
            opening.end
        );
        assert!(
            opening.duration() > 5.0,
            "{} 片头区间退化成了 {:.2}s",
            f.name(),
            opening.duration()
        );

        let expected_ending_start = total - EXPECTED_OUTRO_SECS;
        assert!(
            (ending.start - expected_ending_start).abs() < DETECT_TOLERANCE,
            "{} 片尾起点应接近 {:.2}s，实际 {:.2}s",
            f.name(),
            expected_ending_start,
            ending.start
        );
        // 片尾终点同样会被哈希窗口向内偏，所以只能要求「离文件末尾不太远」+
        // 「没有越过文件末尾」；越过末尾说明区间算错了，必须拦下来
        assert!(
            ending.end <= total + 0.5,
            "{} 片尾终点 {:.2}s 越过了视频总长 {:.2}s",
            f.name(),
            ending.end,
            total
        );
        assert!(
            ending.end >= total - (HASH_QUANTUM_SECS + 2.0),
            "{} 片尾终点离文件末尾太远：{:.2}s vs {:.2}s",
            f.name(),
            ending.end,
            total
        );
        assert!(
            ending.duration() > 3.0,
            "{} 片尾区间退化成了 {:.2}s",
            f.name(),
            ending.duration()
        );
    }

    // 上面那些断言各自看单集，下面这两条是**跨集对照** —— 它们才是真正能抓住
    // 「结果和文件对错位了」的判据：所有集共用同一段片头，所以检测出的片头起点
    // 必须彼此接近；片尾相对文件末尾的位置也固定，所以各集片尾起点之差必须等于
    // 各集总长之差。一旦哪一集的结果串到别的文件上，这两条立刻炸。
    {
        let first_total = files[0].duration_secs.expect("已断言");
        let first_opening = detections[0].opening.expect("已断言").start;
        let first_ending = detections[0].ending.expect("已断言").start;
        for (f, d) in files.iter().zip(detections.iter()).skip(1) {
            let total = f.duration_secs.expect("已断言");
            let opening = d.opening.expect("已断言").start;
            let ending = d.ending.expect("已断言").start;
            assert!(
                (opening - first_opening).abs() < 2.0,
                "{} 的片头起点 {:.2}s 和第一集的 {:.2}s 差太多 —— 结果可能串集了",
                f.name(),
                opening,
                first_opening
            );
            assert!(
                ((ending - first_ending) - (total - first_total)).abs() < 2.0,
                "{} 的片尾起点与总长的位移不一致（{:.2}s vs {:.2}s）",
                f.name(),
                ending - first_ending,
                total - first_total
            );
        }
    }

    // ---- 4. 生成切割计划 --------------------------------------------
    let tasks = pipeline::build_tasks(&files, &detections, &out_dir, "_trimmed");
    assert_eq!(tasks.len(), files.len());
    for t in &tasks {
        assert_eq!(
            t.kind,
            TaskKind::Ready,
            "{} 本该可切割，实际是 {}",
            t.file.name(),
            t.kind.label()
        );
        assert!(
            !t.keep.is_empty(),
            "{} 算出来的保留区间是空的",
            t.file.name()
        );

        let total = t.file.duration_secs.expect("已断言");
        // 找到最长的那一段 —— 它应该就是正片本体
        let main = t
            .keep
            .iter()
            .max_by(|a, b| {
                a.duration()
                    .partial_cmp(&b.duration())
                    .expect("时长不会是 NaN")
            })
            .expect("keep 非空");
        assert!(
            (main.start - EXPECTED_INTRO_SECS).abs() < DETECT_TOLERANCE,
            "{} 正片开头应接近 {EXPECTED_INTRO_SECS}s，实际 {:.2}s",
            t.file.name(),
            main.start
        );
        assert!(
            (main.end - (total - EXPECTED_OUTRO_SECS)).abs() < DETECT_TOLERANCE,
            "{} 正片结尾应接近 {:.2}s，实际 {:.2}s",
            t.file.name(),
            total - EXPECTED_OUTRO_SECS,
            main.end
        );

        // 由于 needle 边界的向内偏移，片头开头和片尾末尾会各剩一小段。
        // 断言的是「它们确实很短」—— 偏移要是突然变成几十秒，这里会立刻炸。
        for seg in &t.keep {
            if std::ptr::eq(seg, main) {
                continue;
            }
            assert!(
                seg.duration() <= HASH_QUANTUM_SECS + DETECT_TOLERANCE + 2.0,
                "{} 残留碎片过长（{:.2}s），边界偏移超出预期",
                t.file.name(),
                seg.duration()
            );
        }

        let removed = t.removed_secs().expect("有时长就该算得出去掉多少");
        let ideal_removed = EXPECTED_INTRO_SECS + EXPECTED_OUTRO_SECS;
        assert!(
            removed >= ideal_removed - 4.0 * HASH_QUANTUM_SECS,
            "{} 去掉的时长只有 {removed:.2}s，明显偏少",
            t.file.name()
        );
        assert!(
            removed <= ideal_removed + 0.5,
            "{} 去掉的时长 {removed:.2}s 超过了片头+片尾的总长 {ideal_removed}s —— 切到正片了",
            t.file.name()
        );
        println!(
            "{}  保留 {}（保留 {} 段，去掉 {:.1}s）",
            t.file.name(),
            pipeline::describe_keep(&t.keep),
            t.keep.len(),
            removed
        );
    }

    let total_planned = pipeline::total_actionable_secs(&tasks);
    println!("计划共产出 {:.1}s 内容", total_planned);

    // ---- 5. 批量切割 ------------------------------------------------
    let cutter = Cutter::new(tools.ffmpeg.clone());
    let mut sink = CollectSink::default();
    let summary = pipeline::cut_tasks(
        &cutter,
        &tasks,
        // 用快速模式：验证的正是流复制那条路径（也是默认路径）
        CutMode::Fast,
        false,
        &cancel,
        &mut sink,
    );
    println!(
        "切割汇总：成功 {}，失败 {}，跳过 {}，产出 {:.1}s",
        summary.succeeded, summary.failed, summary.skipped, summary.produced_secs
    );
    println!("切割过程共收到 {} 行日志", sink.lines.len());

    assert_eq!(summary.failed, 0, "不该有失败的：{:#?}", sink.finished);
    assert_eq!(summary.skipped, 0, "不该有跳过的");
    assert_eq!(summary.succeeded, tasks.len(), "每一集都该切出来");
    assert_eq!(sink.finished.len(), tasks.len());

    // 进度回调必须真的跑起来了。为 0 说明 `-progress pipe:1` 的解析出了问题，
    // 而这是「界面上进度条永远不动」那类 bug 的唯一防线。
    assert!(
        !sink.progress.is_empty(),
        "没有收到任何进度回调，ffmpeg 进度解析可能失效了。\
         日志尾部：{:#?}",
        sink.lines.iter().rev().take(10).collect::<Vec<_>>()
    );
    println!("收到 {} 次进度回调", sink.progress.len());

    // ---- 6. 验证产物 ------------------------------------------------
    for (i, t) in tasks.iter().enumerate() {
        assert!(t.output.is_file(), "输出文件不存在：{}", t.output.display());

        let info = tools
            .probe(&t.output)
            .unwrap_or_else(|e| panic!("探测产物 {} 失败：{e}", t.output.display()));
        let planned = t.kept_secs().unwrap();
        println!(
            "产物 {}  实际 {:.2}s  计划 {:.2}s  差 {:.2}s",
            t.output.file_name().unwrap().to_string_lossy(),
            info.duration_secs,
            planned,
            info.duration_secs - planned
        );

        // 流复制模式的起点会吸附到关键帧，产物只会**变长**（多留一点），
        // 所以差值应当是小的非负数。差成负数说明切多了 —— 那是真 bug。
        //
        // 上界随「保留段数」放大：每一段各自吸附一次，素材的关键帧间隔是 1 秒，
        // 所以 N 段最多多出 N 秒。用固定的 2 秒卡会冤枉多段拼接那条路径。
        let delta = info.duration_secs - planned;
        let slack = t.keep.len() as f64 * KEYFRAME_INTERVAL_SECS + 0.5;
        assert!(
            delta >= -0.5,
            "第 {i} 集产物比计划短了 {:.2}s，正片被切掉了",
            -delta
        );
        assert!(
            delta <= slack,
            "第 {i} 集产物比计划长了 {delta:.2}s，超出 {slack:.1}s 的关键帧吸附上限"
        );
        assert!(info.has_audio, "第 {i} 集产物丢了音轨 —— -map 0 没生效？");
        assert!(
            info.duration_secs < t.file.duration_secs.unwrap() - 10.0,
            "第 {i} 集产物几乎和源文件一样长，说明根本没切掉什么"
        );
    }

    // ---- 7. 验证源目录没有被弄脏 ------------------------------------
    for (path, (len_before, mtime_before)) in &source_state_before {
        let m = std::fs::metadata(path).expect("源文件不见了");
        assert_eq!(m.len(), *len_before, "源文件 {} 大小变了", path.display());
        assert_eq!(
            m.modified().expect("拿不到修改时间"),
            *mtime_before,
            "源文件 {} 的修改时间变了",
            path.display()
        );
    }
    let sidecars_after = count_sidecars(&clips_dir);
    assert_eq!(
        sidecars_after, sidecars_before,
        "源目录残留了 needle 的旁挂文件（之前 {sidecars_before} 个，现在 {sidecars_after} 个）\
         —— 清理逻辑没生效，这会污染用户的剧集目录"
    );

    println!("\n=== 端到端全部通过 ===");
}
