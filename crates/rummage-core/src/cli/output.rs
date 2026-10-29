//! 命令行的输出：给人看的中文文本，以及 `--json`（字段同 `rummage-proto`，带协议版本号 `protocol`）。
//! JSON 把非 ASCII 字符转义为 `\uXXXX`：Windows PowerShell 5.1 按控制台代码页（中文系统为 936）解码外部程序的输出，
//! UTF-8 的中文会乱码，纯 ASCII 在任何代码页下都不变，`ConvertFrom-Json` 再还原成中文。
use std::fmt::Write as _;
use std::io;
use std::path::Path;

use chrono::{Local, TimeZone};
use rummage_proto::{
    FileInfo, IndexState, IndexStatus, PROTOCOL_VERSION, PauseReason, RootInfo, SearchHit,
    SegmentPreview, SemanticUnavailable, tr, trs,
};
use serde::Serialize;

use crate::manage::{clock, progress_pct};

pub struct Results<'a> {
    pub hits: &'a [SearchHit],
    pub took_ms: u64,
    pub partial: bool,
    pub semantic_unavailable: bool,
    pub unavailable: Option<SemanticUnavailable>,
    /// 没有显示的匹配度较低的结果数；只数核心按数量上限返回的那些，实际可能更多，所以是“至少”
    pub weak_omitted: usize,
    /// 以图搜图
    pub image: bool,
}

pub struct Status<'a> {
    pub core_version: &'a str,
    pub onboarded: bool,
    pub index: &'a IndexStatus,
    pub roots: &'a [RootInfo],
}

/// 非 ASCII 字符写成 `\uXXXX`（超出基本平面的写成代理对），其余同 serde_json 的紧凑格式
struct Ascii;

impl serde_json::ser::Formatter for Ascii {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for c in fragment.chars() {
            if c.is_ascii() {
                w.write_all(&[c as u8])?;
                continue;
            }
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                write!(w, "\\u{u:04x}")?;
            }
        }
        Ok(())
    }
}

pub fn ascii_json(v: &impl Serialize) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Ascii);
    v.serialize(&mut ser)
        .expect("输出的值只含字符串键的映射和可序列化的值，序列化不会失败");
    String::from_utf8(out).expect("非 ASCII 字符都已转义")
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "document" => trs!("文档", "문서", "document"),
        "image" => trs!("图片", "이미지", "image"),
        "audio" => trs!("音频", "오디오", "audio"),
        "video" => trs!("视频", "동영상", "video"),
        _ => trs!("其他", "기타", "other"),
    }
}

pub fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    for unit in ["KB", "MB", "GB"] {
        if v < 1024.0 {
            return format!("{v:.1} {unit}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

/// 本地日期，形如 2026-01-05
pub(crate) fn format_date(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(String::new, |t| t.format("%Y-%m-%d").to_string())
}

/// “3”“3–4”，由调用方加上单位
fn span(v: &serde_json::Value, start: &str, end: &str) -> Option<String> {
    let a = v.get(start)?.as_u64().filter(|n| *n > 0)?;
    Some(match v.get(end).and_then(|e| e.as_u64()) {
        Some(b) if b != a => format!("{a}–{b}"),
        _ => a.to_string(),
    })
}

/// 命中位置（同界面 `src/lib/format.ts` 的 `locatorText`），没有可显示的位置时为空
pub fn locator_text(locator_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(locator_json) else {
        return String::new();
    };
    let time = match (
        v.get("start_ms").and_then(|t| t.as_u64()),
        v.get("end_ms").and_then(|t| t.as_u64()),
    ) {
        (Some(a), Some(b)) => Some(format!("{}–{}", clock(a), clock(b))),
        _ => None,
    };
    if let Some(e) = v.get("embed") {
        let what = match e.get("kind").and_then(|k| k.as_str()) {
            Some("image") => trs!("图片", "이미지", "image"),
            Some("audio") => trs!("音频", "오디오", "audio"),
            Some("video") => trs!("视频", "동영상", "video"),
            _ => trs!("内容", "콘텐츠", "media"),
        };
        let at = if let Some(p) = e.get("page").and_then(|p| p.as_u64()) {
            tr!(
                "第 {p} 页中的{what}",
                "{p}페이지의 {what}",
                "{what} on page {p}"
            )
        } else if let Some(p) = e.get("slide").and_then(|p| p.as_u64()) {
            tr!(
                "第 {p} 张幻灯片中的{what}",
                "슬라이드 {p}의 {what}",
                "{what} on slide {p}"
            )
        } else if let Some(s) = e.get("sheet").and_then(|s| s.as_str()) {
            tr!(
                "工作表“{s}”中的{what}",
                "‘{s}’ 시트의 {what}",
                "{what} in sheet \"{s}\""
            )
        } else {
            tr!(
                "文档中的{what}",
                "문서 안의 {what}",
                "{what} in the document"
            )
        };
        return [Some(at), time]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
    }
    if let Some(t) = time {
        return t;
    }
    if v.get("region").is_some() {
        return tr!("长图中的一段", "긴 이미지의 일부", "part of a long image");
    }
    let at = span(&v, "page_start", "page_end")
        .map(|n| tr!("第 {n} 页", "{n}페이지", "page {n}"))
        .or_else(|| {
            span(&v, "slide_start", "slide_end")
                .map(|n| tr!("第 {n} 张幻灯片", "슬라이드 {n}", "slide {n}"))
        });
    let heading = v
        .get("heading")
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty())
        .map(String::from);
    let lines = span(&v, "line_start", "line_end").map(|n| tr!("第 {n} 行", "{n}행", "line {n}"));
    [at, heading, lines]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 语义检索不可用时的说明（同界面的 `unavailableText`）
fn unavailable_text(reason: Option<SemanticUnavailable>, image: bool) -> &'static str {
    match (reason, image) {
        (Some(SemanticUnavailable::Paused), true) => trs!(
            "正在检测设备或下载模型（或尚未完成首次设置），完成后才能以图搜图。",
            "기기를 검사하거나 모델을 다운로드하는 중입니다(또는 처음 설정을 아직 마치지 않았습니다). 끝나야 이미지로 검색할 수 있습니다.",
            "Rummage is checking the device or downloading a model (or first-run setup isn't finished). Search by image works once that's done."
        ),
        (Some(SemanticUnavailable::Paused), false) => trs!(
            "正在检测设备或下载模型（或尚未完成首次设置），完成前只显示文件名或正文中含有搜索词的结果。",
            "기기를 검사하거나 모델을 다운로드하는 중입니다(또는 처음 설정을 아직 마치지 않았습니다). 끝날 때까지는 파일 이름이나 본문에 검색어가 있는 결과만 표시합니다.",
            "Rummage is checking the device or downloading a model (or first-run setup isn't finished). Until then, only results with the search words in the file name or text are shown."
        ),
        (Some(SemanticUnavailable::NoModel), true) => trs!(
            "尚未下载文字模型，无法以图搜图。可在 Rummage 的 设置 → 模型 中下载或导入。",
            "텍스트 모델이 없어 이미지로 검색할 수 없습니다. Rummage의 설정 → 모델에서 다운로드하거나 가져오십시오.",
            "The text model isn't downloaded, so search by image is unavailable. Download or import it in Rummage under Settings → Models."
        ),
        (Some(SemanticUnavailable::NoModel), false) => trs!(
            "尚未下载文字模型，只显示文件名或正文中含有搜索词的结果。可在 Rummage 的 设置 → 模型 中下载或导入。",
            "텍스트 모델이 없어 파일 이름이나 본문에 검색어가 있는 결과만 표시합니다. Rummage의 설정 → 모델에서 다운로드하거나 가져오십시오.",
            "The text model isn't downloaded, so only results with the search words in the file name or text are shown. Download or import it in Rummage under Settings → Models."
        ),
        (Some(SemanticUnavailable::ImagesOff), true) => trs!(
            "尚未选择按内容查找图片，无法以图搜图。可在 Rummage 的 设置 → 文件夹 中勾选“图片”。",
            "이미지를 내용으로 찾도록 설정하지 않아 이미지로 검색할 수 없습니다. Rummage의 설정 → 폴더에서 ‘이미지’를 선택하십시오.",
            "Images aren't indexed by content, so search by image is unavailable. Turn on \"Images\" in Rummage under Settings → Folders."
        ),
        (Some(SemanticUnavailable::NoMmproj), true) => trs!(
            "尚未下载图片与音视频模型，无法以图搜图。可在 Rummage 的 设置 → 模型 中下载或导入。",
            "이미지·오디오·동영상 모델이 없어 이미지로 검색할 수 없습니다. Rummage의 설정 → 모델에서 다운로드하거나 가져오십시오.",
            "The image/audio/video model isn't downloaded, so search by image is unavailable. Download or import it in Rummage under Settings → Models."
        ),
        (Some(SemanticUnavailable::QueryTooLong), true) => trs!(
            "搜索内容过长，请缩短后再试。",
            "검색어가 너무 깁니다. 줄여서 다시 시도하십시오.",
            "The search text is too long. Shorten it and try again."
        ),
        (Some(SemanticUnavailable::QueryTooLong), false) => trs!(
            "搜索内容过长，只显示文件名或正文中含有搜索词的结果。请缩短后再试。",
            "검색어가 너무 길어 파일 이름이나 본문에 검색어가 있는 결과만 표시합니다. 줄여서 다시 시도하십시오.",
            "The search text is too long, so only results with the search words in the file name or text are shown. Shorten it and try again."
        ),
        (_, true) => trs!(
            "内容识别无法使用，无法以图搜图。请重新启动 Rummage。",
            "내용 인식을 쓸 수 없어 이미지로 검색할 수 없습니다. Rummage를 다시 시작하십시오.",
            "Content recognition is unavailable, so search by image doesn't work. Restart Rummage."
        ),
        (_, false) => trs!(
            "内容识别无法使用，只显示文件名或正文中含有搜索词的结果。",
            "내용 인식을 쓸 수 없어 파일 이름이나 본문에 검색어가 있는 결과만 표시합니다.",
            "Content recognition is unavailable, so only results with the search words in the file name or text are shown."
        ),
    }
}

fn match_text(hit: &SearchHit) -> Vec<&'static str> {
    let mut parts = Vec::new();
    if hit.cosine.is_some() {
        parts.push(trs!("内容相近", "내용 유사", "similar content"));
    }
    if hit.name_hit {
        parts.push(trs!(
            "文件名含搜索词",
            "파일 이름에 검색어 포함",
            "search words in name"
        ));
    }
    if !hit.text_segments.is_empty() {
        parts.push(trs!(
            "正文含搜索词",
            "본문에 검색어 포함",
            "search words in text"
        ));
    }
    parts
}

pub fn search_text(r: &Results) -> String {
    let mut out = String::new();
    if r.semantic_unavailable {
        let _ = writeln!(out, "{}", unavailable_text(r.unavailable, r.image));
    }
    if r.partial {
        let _ = writeln!(out, "{}", partial_text());
    }
    if r.hits.is_empty() {
        out.push_str(trs!(
            "没有找到相关的文件。",
            "관련 파일을 찾지 못했습니다.",
            "No matching files found."
        ));
    } else {
        let n = r.hits.len();
        let _ = writeln!(
            out,
            "{}",
            tr!("找到 {n} 个结果：", "결과 {n}개:", "{n} results:")
        );
    }
    for (i, h) in r.hits.iter().enumerate() {
        let mut head = format!("{}. {}  {}", i + 1, h.name, kind_label(&h.kind));
        if let Some(p) = h.match_pct {
            let _ = write!(
                head,
                "  {}",
                tr!("匹配度 {p}%", "일치도 {p}%", "{p}% match")
            );
        }
        let _ = write!(out, "\n{head}\n   {}\n", h.path);
        let mut info = vec![
            tr!(
                "修改于 {}",
                "수정일 {}",
                "modified {}",
                format_date(h.mtime)
            ),
            format_size(h.size),
        ];
        if h.missing {
            info.push(tr!(
                "找不到所在的文件夹",
                "폴더를 찾을 수 없음",
                "folder not found"
            ));
        } else if h.offline {
            info.push(offline_text().into());
        } else if h.cloud {
            info.push(tr!("仅在云端", "온라인 전용 파일", "online-only file"));
        }
        info.extend(match_text(h).into_iter().map(String::from));
        if let Some(s) = &h.snippet {
            info.push(tr!("命中 ID {}", "결과 ID {}", "hit ID {}", s.segment_id));
        }
        let _ = write!(out, "   {}", info.join(" · "));
        // 位置单独一行，免得没有匹配度时被看成匹配度
        if let Some(loc) = h.segments.first().map(|s| locator_text(&s.locator_json))
            && !loc.is_empty()
        {
            let _ = write!(
                out,
                "\n   {}",
                tr!("位置：{loc}", "위치: {loc}", "Location: {loc}")
            );
        }
        if let Some(s) = &h.snippet {
            let _ = write!(out, "\n   “{}”", s.text);
        }
        out.push('\n');
    }
    if r.weak_omitted > 0 {
        let n = r.weak_omitted;
        let _ = write!(
            out,
            "\n{}",
            tr!(
                "另有至少 {n} 个匹配度较低的结果，加 --include-weak 显示。",
                "일치도가 낮은 결과가 {n}개 이상 더 있습니다. --include-weak를 붙이면 표시합니다.",
                "At least {n} more lower-match results were left out; add --include-weak to show them."
            )
        );
    }
    out.trim_end().to_string()
}

/// `search --json` 的输出，字段同 `Reply::Results`，另加协议版本与省略的低相关结果数
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchOut<'a> {
    protocol: u32,
    hits: &'a [SearchHit],
    took_ms: u64,
    partial: bool,
    semantic_unavailable: bool,
    unavailable: Option<SemanticUnavailable>,
    /// 没有列出的匹配度较低的结果，至少这么多（只数核心按数量上限返回的那些）
    weak_omitted: usize,
}

pub fn search_json(r: &Results) -> String {
    ascii_json(&SearchOut {
        protocol: PROTOCOL_VERSION,
        hits: r.hits,
        took_ms: r.took_ms,
        partial: r.partial,
        semantic_unavailable: r.semantic_unavailable,
        unavailable: r.unavailable,
        weak_omitted: r.weak_omitted,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportOut<'a> {
    protocol: u32,
    path: &'a Path,
    count: usize,
    partial: bool,
    semantic_unavailable: bool,
    unavailable: Option<SemanticUnavailable>,
    weak_omitted: usize,
}

/// `--output` 保存后输出的说明，提示与结果文本相同
pub fn export_text(r: &Results, path: &Path, json: bool) -> String {
    if json {
        return ascii_json(&ExportOut {
            protocol: PROTOCOL_VERSION,
            path,
            count: r.hits.len(),
            partial: r.partial,
            semantic_unavailable: r.semantic_unavailable,
            unavailable: r.unavailable,
            weak_omitted: r.weak_omitted,
        });
    }
    let mut out = String::new();
    if r.semantic_unavailable {
        let _ = writeln!(out, "{}", unavailable_text(r.unavailable, r.image));
    }
    if r.partial {
        let _ = writeln!(out, "{}", partial_text());
    }
    let n = r.hits.len();
    let _ = write!(
        out,
        "{}",
        tr!(
            "已将 {n} 个结果保存到 \"{}\"。",
            "결과 {n}개를 \"{}\"에 저장했습니다.",
            "Saved {n} results to \"{}\".",
            path.display()
        )
    );
    if r.weak_omitted > 0 {
        let n = r.weak_omitted;
        let _ = write!(
            out,
            "\n{}",
            tr!(
                "另有至少 {n} 个匹配度较低的结果没有保存，加 --include-weak 一并保存。",
                "일치도가 낮은 결과 {n}개 이상은 저장하지 않았습니다. --include-weak를 붙이면 함께 저장합니다.",
                "At least {n} more lower-match results were not saved; add --include-weak to save them too."
            )
        );
    }
    out
}

/// `--folder` 不是已索引的文件夹时的错误，列出全部已索引的文件夹
pub fn not_a_root(folder: &Path, roots: &[RootInfo]) -> String {
    if roots.is_empty() {
        return tr!(
            "\"{}\" 不是已索引的文件夹。还没有已索引的文件夹，请先在 Rummage 中添加。",
            "\"{}\" 폴더는 색인 폴더가 아닙니다. 아직 색인 폴더가 없으니 Rummage에서 먼저 추가하십시오.",
            "\"{}\" is not an indexed folder. There are no indexed folders yet; add one in Rummage first.",
            folder.display()
        );
    }
    let list: Vec<_> = roots.iter().map(|r| format!("  {}", r.path)).collect();
    tr!(
        "\"{}\" 不是已索引的文件夹。已索引的文件夹：\n{}",
        "\"{}\" 폴더는 색인 폴더가 아닙니다. 색인 폴더:\n{}",
        "\"{}\" is not an indexed folder. Indexed folders:\n{}",
        folder.display(),
        list.join("\n")
    )
}

/// `search --folder` 不在任何已索引的文件夹中时的错误，列出全部已索引的文件夹
pub fn not_in_root(folder: &Path, roots: &[RootInfo]) -> String {
    if roots.is_empty() {
        return tr!(
            "\"{}\" 不在已索引的文件夹中。还没有已索引的文件夹，请先在 Rummage 中添加。",
            "\"{}\" 폴더는 색인 폴더 안에 있지 않습니다. 아직 색인 폴더가 없으니 Rummage에서 먼저 추가하십시오.",
            "\"{}\" is not inside an indexed folder. There are no indexed folders yet; add one in Rummage first.",
            folder.display()
        );
    }
    let list: Vec<_> = roots.iter().map(|r| format!("  {}", r.path)).collect();
    tr!(
        "\"{}\" 不在已索引的文件夹中。已索引的文件夹：\n{}",
        "\"{}\" 폴더는 색인 폴더 안에 있지 않습니다. 색인 폴더:\n{}",
        "\"{}\" is not inside an indexed folder. Indexed folders:\n{}",
        folder.display(),
        list.join("\n")
    )
}

pub(crate) fn pause_text(p: &PauseReason) -> String {
    match p {
        PauseReason::Manual { until: None } => {
            tr!("已手动暂停", "직접 일시정지함", "paused manually")
        }
        PauseReason::Manual { until: Some(t) } => {
            let at = Local
                .timestamp_millis_opt(*t)
                .single()
                .map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string());
            tr!(
                "已手动暂停，到 {at} 恢复",
                "직접 일시정지함, {at}에 계속",
                "paused manually until {at}"
            )
        }
        PauseReason::Sleeping => tr!(
            "系统即将睡眠或刚唤醒，稍后继续",
            "시스템이 곧 잠자기에 들어가거나 막 깨어남, 잠시 후 계속",
            "the system is about to sleep or just woke up; resumes shortly"
        ),
        PauseReason::DiskLow { .. } => tr!(
            "磁盘空间不足，已暂停",
            "디스크 공간 부족으로 일시정지됨",
            "paused: low disk space"
        ),
        PauseReason::Quota => tr!(
            "已达到系统每天 6 小时的后台运行上限",
            "시스템의 하루 6시간 백그라운드 실행 한도에 도달함",
            "reached the system's 6-hour daily background limit"
        ),
        PauseReason::Hot => tr!(
            "手机温度过高，已暂停",
            "휴대폰 온도가 너무 높아 일시정지됨",
            "paused: the phone is too hot"
        ),
        PauseReason::Battery => tr!(
            "未接电源，已暂停",
            "전원이 연결되지 않아 일시정지됨",
            "paused: not plugged in"
        ),
        PauseReason::LowBattery => tr!(
            "电量低，已暂停",
            "배터리 부족으로 일시정지됨",
            "paused: low battery"
        ),
        PauseReason::ScreenOn => tr!(
            "屏幕亮着，已暂停",
            "화면이 켜져 있어 일시정지됨",
            "paused: the screen is on"
        ),
        PauseReason::Fullscreen => tr!(
            "有全屏程序在运行，已暂停",
            "전체 화면 프로그램이 실행 중이라 일시정지됨",
            "paused: a full-screen program is running"
        ),
    }
}

/// 剩余时间（同界面的 `formatDuration`）
fn duration_text(sec: u64) -> String {
    if sec < 60 {
        return tr!("不到 1 分钟", "1분 미만", "less than 1 minute");
    }
    let min = (sec as f64 / 60.0).round() as u64;
    if min < 60 {
        return tr!("约 {min} 分钟", "약 {min}분", "about {min} min");
    }
    if min < 24 * 60 {
        let (h, m) = (min / 60, min % 60);
        return match m {
            0 => tr!("约 {h} 小时", "약 {h}시간", "about {h} h"),
            _ => tr!(
                "约 {h} 小时 {m} 分钟",
                "약 {h}시간 {m}분",
                "about {h} h {m} min"
            ),
        };
    }
    let (d, h) = (min / 1440, min / 60 % 24);
    match h {
        0 => tr!("约 {d} 天", "약 {d}일", "about {d} d"),
        _ => tr!(
            "约 {d} 天 {h} 小时",
            "약 {d}일 {h}시간",
            "about {d} d {h} h"
        ),
    }
}

fn partial_text() -> &'static str {
    trs!(
        "索引尚未完成，结果可能不完整。",
        "색인이 아직 끝나지 않아 결과가 불완전할 수 있습니다.",
        "Indexing isn't finished, so results may be incomplete."
    )
}

pub(crate) fn offline_text() -> &'static str {
    trs!("设备未连接", "기기 연결 안 됨", "device not connected")
}

pub(crate) fn folder_missing_text() -> &'static str {
    trs!("找不到文件夹", "폴더를 찾을 수 없음", "folder not found")
}

pub(crate) fn files_text(n: i64) -> String {
    tr!("{n} 个文件", "파일 {n}개", "{n} files")
}

/// 括号中的附注：中文用全角括号，紧跟前文；其他语言用半角括号，前面空一格
pub(crate) fn paren(note: &str) -> String {
    tr!("（{note}）", " ({note})", " ({note})")
}

pub fn status_text(s: &Status) -> String {
    let i = s.index;
    let mut out = format!("Rummage {}\n", s.core_version);
    if !s.onboarded {
        out.push_str(trs!(
            "尚未完成首次设置，请先打开 Rummage。\n",
            "처음 설정을 아직 마치지 않았습니다. 먼저 Rummage를 여십시오.\n",
            "First-run setup isn't finished. Open Rummage first.\n"
        ));
    }
    let state = match i.state {
        IndexState::Idle if i.pending == 0 => tr!("索引已完成", "색인 완료", "indexing complete"),
        IndexState::Idle | IndexState::Running => tr!("正在建立索引", "색인 생성 중", "indexing"),
        IndexState::NoModel => tr!(
            "缺少文字模型，请在 Rummage 中下载或导入",
            "텍스트 모델 없음, Rummage에서 다운로드하거나 가져오십시오",
            "text model missing; download or import it in Rummage"
        ),
        IndexState::NoMmproj => tr!(
            "缺少图片与音视频模型，请在 Rummage 中下载或导入",
            "이미지·오디오·동영상 모델 없음, Rummage에서 다운로드하거나 가져오십시오",
            "image/audio/video model missing; download or import it in Rummage"
        ),
        // 后面可能接括号，去掉末尾的句号；兜底与界面的 IndexHint 一致
        IndexState::Error => crate::cli::plain(i.error.as_deref().unwrap_or(trs!(
            "内容识别无法启动，目前只能按文件名搜索。请重新启动 Rummage，或在 Rummage 的 设置 → 设备 中选择使用 CPU。",
            "내용 인식을 시작할 수 없어 지금은 파일 이름으로만 검색할 수 있습니다. Rummage를 다시 시작하거나 Rummage의 설정 → 기기에서 CPU를 선택하십시오.",
            "Content recognition could not start, so only file names can be searched for now. Restart Rummage, or choose CPU in Rummage under Settings → Device.",
        )))
        .trim_end_matches(['。', '.'])
        .to_string(),
    };
    let _ = write!(
        out,
        "{}",
        tr!("状态：{state}", "상태: {state}", "Status: {state}")
    );
    if let Some(p) = &i.paused {
        let _ = write!(out, "{}", paren(&pause_text(p)));
    }
    let pct = progress_pct(i);
    let (n, total, failed) = (i.done + i.failed, i.total, i.failed);
    let _ = write!(
        out,
        "\n{}",
        tr!(
            "进度：{n} / {total}（{pct}%），失败 {failed}",
            "진행: {n} / {total} ({pct}%), 실패 {failed}",
            "Progress: {n} / {total} ({pct}%), {failed} failed"
        )
    );
    if let Some(eta) = i.eta_s {
        let left = duration_text(eta);
        let _ = write!(
            out,
            "{}",
            tr!("，剩余{left}", ", 남은 시간 {left}", ", {left} left")
        );
    }
    for (kind, c) in &i.by_kind {
        let _ = write!(
            out,
            "\n  {} {} / {}",
            kind_label(kind),
            c.done + c.failed,
            c.total
        );
    }
    if let Some(r) = &i.rebuild {
        let _ = write!(
            out,
            "\n{}",
            tr!(
                "按新设置重建：{} / {}",
                "새 설정으로 다시 만드는 중: {} / {}",
                "Rebuilding with new settings: {} / {}",
                r.done,
                r.total
            )
        );
    }
    let backend = i
        .backend
        .clone()
        .unwrap_or_else(|| trs!("未启动", "시작 안 됨", "not started").into());
    let _ = write!(
        out,
        "\n{}",
        tr!(
            "内容识别设备：{backend}",
            "내용 인식 기기: {backend}",
            "Recognition device: {backend}"
        )
    );
    if s.roots.is_empty() {
        out.push_str(trs!(
            "\n还没有已索引的文件夹，请在 Rummage 中添加。",
            "\n아직 색인 폴더가 없습니다. Rummage에서 추가하십시오.",
            "\nNo indexed folders yet. Add one in Rummage."
        ));
        return out;
    }
    out.push_str(trs!(
        "\n已索引的文件夹：",
        "\n색인 폴더:",
        "\nIndexed folders:"
    ));
    for r in s.roots {
        let note = if r.missing {
            folder_missing_text().into()
        } else if r.offline {
            offline_text().into()
        } else {
            files_text(r.files)
        };
        let _ = write!(out, "\n  {}{}", r.path, paren(&note));
    }
    out
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOut<'a> {
    protocol: u32,
    core_version: &'a str,
    onboarded: bool,
    index: &'a IndexStatus,
    roots: &'a [RootInfo],
}

pub fn status_json(s: &Status) -> String {
    ascii_json(&StatusOut {
        protocol: PROTOCOL_VERSION,
        core_version: s.core_version,
        onboarded: s.onboarded,
        index: s.index,
        roots: s.roots,
    })
}

pub fn context_text(file: Option<&FileInfo>, seg: &SegmentPreview) -> String {
    let mut out = String::new();
    let loc = locator_text(&seg.locator_json);
    if let Some(f) = file {
        out.push_str(&f.name);
        if !loc.is_empty() {
            out.push_str(&paren(&loc));
        }
        let _ = write!(out, "\n{}\n", f.path);
    } else if !loc.is_empty() {
        let _ = writeln!(out, "{loc}");
    }
    for (label, text) in [
        (trs!("前文", "앞 내용", "before"), &seg.before),
        (trs!("命中", "결과", "hit"), &seg.text),
        (trs!("后文", "뒤 내용", "after"), &seg.after),
    ] {
        if let Some(t) = text {
            let _ = write!(out, "\n[{label}]\n{}\n", t.trim());
        }
    }
    out.trim_end().to_string()
}

#[derive(Serialize)]
struct ContextOut<'a> {
    protocol: u32,
    file: Option<&'a FileInfo>,
    segment: &'a SegmentPreview,
}

pub fn context_json(file: Option<&FileInfo>, seg: &SegmentPreview) -> String {
    ascii_json(&ContextOut {
        protocol: PROTOCOL_VERSION,
        file,
        segment: seg,
    })
}
