//! 命令行中查看与操作应用的子命令：`folders`、`settings`、`pause`、`resume`、`failed`，
//! 与 MCP 的 `list_folders` 等工具一一对应，判断逻辑见 `crate::manage`。
//! - 文件夹路径的相对路径按本进程的当前目录转成绝对路径；移除、修改时必须是已索引的文件夹（同 `--folder`）。
//! - 添加的校验（必须是存在的本地文件夹、不能与已有的互相包含等）由核心做，与界面添加时一致。
//! - 输出：中文文本，或 `--json`（带协议版本号 `protocol`，非 ASCII 字符转义，同 `output`）。
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use rummage_proto::{
    FailedFile, Group, IndexStatus, PROTOCOL_VERSION, ParamDef, ParamKind, ParamValue, Reply,
    Request, RootInfo, params, tr, trs, whole_drive,
};
use serde::Serialize;

use rummage_proto::i18n::{Lang, lang};

use super::output::{
    ascii_json, files_text, folder_missing_text, kind_label, offline_text, paren, pause_text,
};
use super::{
    Args, Cmd, Outcome, Session, bad_path, emit, missing_value, parse_kinds, print_failed,
    unexpected, unknown_arg, unknown_kind, utf8,
};
use crate::manage::{
    CONTENT_KINDS, MAX_PAUSE_MIN, current, enabled_kinds, find_root, kind_changes, parse_value,
    progress_pct, rebuild_kinds, value_text,
};

/// `failed` 最多列出的文件数（最近失败的在前）
const FAILED_SHOWN: u32 = 100;

/// `cmd` 为 `folders`、`settings`、`pause`、`resume` 或 `failed`
pub(super) fn parse(cmd: &str, args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut args = args.into_iter();
    let mut json = false;
    let mut data_dir = None;
    let mut words = Vec::new();
    let mut kinds: Option<Vec<String>> = None;
    let mut excludes: Option<Vec<String>> = None;
    let mut minutes = None;
    let mut no_exclude = false;
    while let Some(arg) = args.next() {
        let flag = arg.to_string_lossy().into_owned();
        if !flag.starts_with("--") {
            words.push(utf8(arg, trs!("参数", "인수", "argument"))?);
            continue;
        }
        if flag == "--json" {
            json = true;
            continue;
        }
        // 清空排除规则；Windows PowerShell 5.1 不把空字符串参数传给外部程序，`--exclude ""` 在那里用不了
        if flag == "--no-exclude" && cmd == "folders" {
            no_exclude = true;
            excludes.get_or_insert_default();
            continue;
        }
        let known = flag == "--data-dir"
            || (cmd == "folders" && matches!(flag.as_str(), "--types" | "--exclude"))
            || (cmd == "pause" && flag == "--minutes");
        if !known {
            return Err(unknown_arg(&flag));
        }
        let value = args.next().ok_or_else(|| missing_value(&flag))?;
        match flag.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value)),
            "--types" => {
                let all: Vec<_> = CONTENT_KINDS.iter().map(|c| c.0).collect();
                let list = kinds.get_or_insert_default();
                for k in parse_kinds(utf8(value, &flag)?.split([',', '，']), &all)
                    .map_err(|k| unknown_kind(&k, &all))?
                {
                    if !list.contains(&k) {
                        list.push(k);
                    }
                }
            }
            "--exclude" => excludes.get_or_insert_default().push(utf8(value, &flag)?),
            _ => {
                minutes = Some(
                    utf8(value, &flag)?
                        .trim()
                        .parse()
                        .ok()
                        .filter(|n| (1..=MAX_PAUSE_MIN).contains(n))
                        .ok_or_else(|| {
                            tr!(
                                "--minutes 应为 1–{MAX_PAUSE_MIN} 的整数。",
                                "--minutes는 1–{MAX_PAUSE_MIN} 사이의 정수여야 합니다.",
                                "--minutes must be a whole number from 1 to {MAX_PAUSE_MIN}."
                            )
                        })?,
                );
            }
        }
    }
    if kinds.as_ref().is_some_and(|k| k.is_empty()) {
        return Err(tr!(
            "--types 至少给出一种类型。",
            "--types에는 유형을 하나 이상 지정해야 합니다.",
            "--types needs at least one type."
        ));
    }
    if no_exclude && excludes.as_ref().is_some_and(|e| !e.is_empty()) {
        return Err(tr!(
            "--no-exclude 与 --exclude 不能同时使用。",
            "--no-exclude와 --exclude는 함께 쓸 수 없습니다.",
            "--no-exclude and --exclude cannot be used together."
        ));
    }
    let mut words = words.into_iter();
    let sub = words.next();
    let mut path = |what: &str| {
        words.next().map(PathBuf::from).ok_or_else(|| {
            tr!(
                "folders {what} 需要一个文件夹路径。",
                "folders {what}에는 폴더 경로가 필요합니다.",
                "folders {what} needs a folder path."
            )
        })
    };
    let cmd = match (cmd, sub.as_deref()) {
        ("folders", None) => Cmd::Folders,
        ("folders", Some("add")) => Cmd::AddFolder {
            path: path("add")?,
            kinds: kinds.take(),
            excludes: excludes.take(),
        },
        ("folders", Some("set")) => {
            let path = path("set")?;
            if kinds.is_none() && excludes.is_none() {
                return Err(tr!(
                    "folders set 需要 --types 或 --exclude。",
                    "folders set에는 --types나 --exclude가 필요합니다.",
                    "folders set needs --types or --exclude."
                ));
            }
            Cmd::SetFolder {
                path,
                kinds: kinds.take(),
                excludes: excludes.take(),
            }
        }
        ("folders", Some("remove")) => Cmd::RemoveFolder(path("remove")?),
        ("settings", None) => Cmd::Settings(None),
        ("settings", Some("set")) => {
            let (Some(key), Some(value)) = (words.next(), words.next()) else {
                return Err(tr!(
                    "settings set 需要参数的键和值，例如 settings set chunk_tokens 256。",
                    "settings set에는 설정 키와 값이 필요합니다(예: settings set chunk_tokens 256).",
                    "settings set needs a setting key and value, e.g. settings set chunk_tokens 256."
                ));
            };
            Cmd::SetSetting { key, value }
        }
        ("settings", Some(key)) => Cmd::Settings(Some(key.to_string())),
        ("pause", None) => Cmd::Pause(minutes),
        ("resume", None) => Cmd::Resume,
        ("failed", None) => Cmd::Failed,
        ("failed", Some("retry")) => Cmd::RetryFailed,
        (_, Some(word)) => return Err(unknown_arg(word)),
        (cmd, None) => unreachable!("main 只把 COMMANDS 中的子命令交给 parse：{cmd}"),
    };
    if let Some(word) = words.next() {
        return Err(unknown_arg(&word));
    }
    if kinds.is_some() || excludes.is_some() {
        return Err(tr!(
            "--types、--exclude、--no-exclude 只能用于 folders add 与 folders set。",
            "--types, --exclude, --no-exclude는 folders add와 folders set에서만 쓸 수 있습니다.",
            "--types, --exclude and --no-exclude only work with folders add and folders set."
        ));
    }
    Ok(Args {
        cmd,
        json,
        data_dir,
    })
}

fn absolute(p: &Path) -> Result<String, String> {
    std::path::absolute(p)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|_| bad_path(p))
}

/// “文档、图片”；一种都没有时为“无（只按文件名搜索）”
fn kinds_text(kinds: &[&str]) -> String {
    if kinds.is_empty() {
        return tr!(
            "无（只按文件名搜索）",
            "없음(파일 이름으로만 검색)",
            "none (search by file name only)"
        );
    }
    kinds
        .iter()
        .map(|k| kind_label(k))
        .collect::<Vec<_>>()
        .join(list_sep())
}

/// 列表的分隔符：中文用顿号
fn list_sep() -> &'static str {
    trs!("、", ", ", ", ")
}

fn excludes_text(rules: &[String]) -> String {
    if rules.is_empty() {
        return tr!("无", "없음", "none");
    }
    rules.join(list_sep())
}

/// 整个磁盘或存储（`whole_drive`）的提醒
const WHOLE_DRIVE_NOTE: &str = "这是整个磁盘或存储。Rummage 不是为全盘索引设计的，系统与程序文件大多只能按文件名搜索，文件过多会拖慢建立索引和搜索，建议用 folders remove 移除它，只添加需要搜索的文件夹。";

fn whole_drive_note() -> String {
    let note = match lang() {
        Lang::Zh => WHOLE_DRIVE_NOTE,
        Lang::Ko => {
            "드라이브나 저장소 전체입니다. Rummage는 전체 드라이브를 색인하도록 만들어지지 않았습니다. 시스템·프로그램 파일은 대부분 파일 이름으로만 검색되고, 파일이 너무 많으면 색인 생성과 검색이 느려집니다. folders remove로 제거하고 검색할 폴더만 추가하는 것이 좋습니다."
        }
        Lang::En => {
            "This is a whole drive or storage. Rummage isn't designed to index whole drives: most system and program files can only be searched by name, and too many files slow down indexing and search. Remove it with folders remove and add only the folders you need to search."
        }
    };
    tr!("注意：{note}", "주의: {note}", "Note: {note}")
}

/// 识别内容的文件类型一行
fn kinds_line(kinds: &str) -> String {
    tr!(
        "识别内容的文件类型：{kinds}（所有文件夹共用）",
        "내용을 인식할 파일 유형: {kinds}(모든 폴더 공통)",
        "Content file types: {kinds} (shared by all folders)"
    )
}

pub fn folders_text(kinds: &[&str], roots: &[RootInfo], index: Option<&IndexStatus>) -> String {
    let mut out = kinds_line(&kinds_text(kinds));
    if let Some(i) = index {
        let pct = progress_pct(i);
        let (n, total, failed) = (i.done + i.failed, i.total, i.failed);
        let _ = write!(
            out,
            "\n{}",
            tr!(
                "索引进度：{n} / {total}（{pct}%），失败 {failed}",
                "색인 진행: {n} / {total} ({pct}%), 실패 {failed}",
                "Indexing progress: {n} / {total} ({pct}%), {failed} failed"
            )
        );
        if let Some(p) = &i.paused {
            let _ = write!(out, "{}{}", trs!("，", ", ", ", "), pause_text(p));
        }
    }
    if roots.is_empty() {
        out.push_str(trs!(
            "\n还没有已索引的文件夹，可用 folders add <文件夹> 添加。",
            "\n아직 색인 폴더가 없습니다. folders add <폴더>로 추가할 수 있습니다.",
            "\nNo indexed folders yet. Add one with folders add <folder>."
        ));
        return out;
    }
    out.push_str(trs!(
        "\n已索引的文件夹：",
        "\n색인 폴더:",
        "\nIndexed folders:"
    ));
    for r in roots {
        let note = if r.missing {
            folder_missing_text().into()
        } else if r.offline {
            offline_text().into()
        } else if r.scanning {
            tr!("正在统计文件", "파일 세는 중", "counting files")
        } else {
            files_text(r.files)
        };
        let _ = write!(out, "\n  {}{}", r.path, paren(&note));
        if !r.excludes.is_empty() {
            let rules = excludes_text(&r.excludes);
            let _ = write!(
                out,
                "\n    {}",
                tr!("排除：{rules}", "제외: {rules}", "Excluded: {rules}")
            );
        }
        if whole_drive(&r.path) {
            let _ = write!(out, "\n    {}", whole_drive_note());
        }
    }
    out
}

/// 排除规则一行
fn rules_line(rules: &[String]) -> String {
    let rules = excludes_text(rules);
    tr!(
        "排除规则：{rules}",
        "제외 규칙: {rules}",
        "Exclude rules: {rules}"
    )
}

fn no_param(key: &str) -> String {
    tr!(
        "没有名为“{key}”的参数，可用 settings 查看全部。",
        "‘{key}’라는 설정이 없습니다. settings로 전체 목록을 확인하십시오.",
        "There is no setting named \"{key}\". Run settings to see them all."
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FoldersOut<'a> {
    protocol: u32,
    kinds: &'a [&'a str],
    roots: &'a [RootInfo],
    index: Option<&'a IndexStatus>,
}

/// 操作的结果：一句说明，添加、修改文件夹时附上该文件夹
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoneOut<'a> {
    protocol: u32,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<&'a RootInfo>,
}

fn done_json(message: &str, root: Option<&RootInfo>) -> String {
    ascii_json(&DoneOut {
        protocol: PROTOCOL_VERSION,
        message,
        root,
    })
}

fn group_text(g: Group) -> &'static str {
    match g {
        Group::Search => trs!(
            "搜索（修改后立即生效）",
            "검색(바로 적용)",
            "Search (applies immediately)"
        ),
        Group::Index => trs!("索引", "색인", "Indexing"),
        Group::Resource => trs!(
            "资源占用（修改后立即生效）",
            "리소스 사용(바로 적용)",
            "Resource usage (applies immediately)"
        ),
        Group::Advanced => trs!("高级", "고급", "Advanced"),
    }
}

/// 允许的取值
pub fn range_text(kind: ParamKind) -> String {
    match kind {
        ParamKind::Bool => tr!("true 或 false", "true 또는 false", "true or false"),
        ParamKind::Int { min, max } => {
            tr!(
                "{min}–{max} 的整数",
                "{min}–{max} 사이의 정수",
                "a whole number from {min} to {max}"
            )
        }
        ParamKind::Float { min, max } => {
            tr!(
                "{min}–{max} 的数",
                "{min}–{max} 사이의 수",
                "a number from {min} to {max}"
            )
        }
        ParamKind::Named { options, .. } => {
            let list: Vec<_> = options
                .iter()
                .map(|o| value_text(kind, ParamValue::Int(o.0), false))
                .collect();
            let list = list.join(list_sep());
            tr!("可选 {list}", "{list} 중 하나", "one of {list}")
        }
    }
}

fn index_kinds_text(kinds: &[&str]) -> String {
    kinds
        .iter()
        .map(|k| kind_label(k))
        .collect::<Vec<_>>()
        .join(list_sep())
}

fn setting_text(out: &mut String, d: &ParamDef, value: ParamValue) {
    let _ = write!(
        out,
        "\n  {} = {}{}",
        d.key,
        value_text(d.kind, value, false),
        tr!(
            "（默认 {}，{}",
            " (기본값 {}, {}",
            " (default {}, {}",
            value_text(d.kind, (d.default)(), false),
            range_text(d.kind)
        )
    );
    if d.affects_index {
        let kinds = index_kinds_text(d.index_kinds);
        let _ = write!(
            out,
            "{}",
            tr!(
                "，修改后重建{kinds}的索引",
                ", 바꾸면 {kinds} 색인을 다시 만듦",
                "; changing it rebuilds the {kinds} index"
            )
        );
    }
    let _ = write!(
        out,
        "{}",
        tr!(
            "）\n    {}：{}",
            ")\n    {}: {}",
            ")\n    {}: {}",
            d.label,
            d.help
        )
    );
}

/// 全部参数，按分组
pub fn settings_text(values: &BTreeMap<String, ParamValue>) -> String {
    let mut out = String::new();
    for g in [
        Group::Search,
        Group::Index,
        Group::Resource,
        Group::Advanced,
    ] {
        let _ = write!(out, "\n{}", group_text(g));
        for d in params::PARAMS.iter().filter(|d| d.group == g && d.shown()) {
            setting_text(&mut out, d, current(values, d));
        }
        out.push('\n');
    }
    out.trim().to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingOut<'a> {
    #[serde(flatten)]
    def: &'a ParamDef,
    value: ParamValue,
}

#[derive(Serialize)]
struct SettingsOut<'a> {
    protocol: u32,
    settings: Vec<SettingOut<'a>>,
}

/// 修改参数后的说明；`changed` 为假时值没有变化，`rebuild` 为要重建的种类
pub fn set_text(d: &ParamDef, value: ParamValue, changed: bool, rebuild: &[&str]) -> String {
    let shown = value_text(d.kind, value, false);
    let (key, label) = (d.key, d.label);
    if !changed {
        return tr!(
            "{key}（{label}）已是 {shown}，没有变化。",
            "{key}({label}) 값은 이미 {shown}입니다. 바뀐 것이 없습니다.",
            "{key} ({label}) is already {shown}; nothing changed."
        );
    }
    let mut out = tr!(
        "已将 {key}（{label}）设为 {shown}。",
        "{key}({label}) 값을 바꿨습니다: {shown}.",
        "Set {key} ({label}) to {shown}."
    );
    if !rebuild.is_empty() {
        let kinds = index_kinds_text(rebuild);
        out.push_str(&tr!(
            "将按新设置重建{kinds}的索引，重建期间旧索引仍可搜索。",
            " 새 설정으로 {kinds} 색인을 다시 만듭니다. 다시 만드는 동안에도 기존 색인으로 검색할 수 있습니다.",
            " The {kinds} index will be rebuilt with the new setting; the old index stays searchable meanwhile."
        ));
    } else if d.affects_index {
        out.push_str(trs!(
            "视频识别方式不是“自定义”，此项暂不生效。",
            " 동영상 인식 방식이 ‘사용자 지정’이 아니어서 이 항목은 지금 적용되지 않습니다.",
            " The video recognition mode isn't \"Custom\", so this has no effect for now."
        ));
    }
    if d.key == "selftest_min_cosine" {
        out.push_str(trs!(
            "新的通过标准在 Rummage 的 设置 → 设备 中重新检测后生效。",
            " 새 통과 기준은 Rummage의 설정 → 기기에서 다시 검사한 뒤 적용됩니다.",
            " The new pass threshold takes effect after the device check is run again in Rummage under Settings → Device."
        ));
    }
    out
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetOut<'a> {
    protocol: u32,
    key: &'a str,
    value: ParamValue,
    changed: bool,
    rebuild: &'a [&'a str],
    message: &'a str,
}

pub fn failed_text(files: &[FailedFile], total: u64, retryable: u64) -> String {
    if total == 0 {
        return tr!(
            "没有建立索引失败的文件。",
            "색인 생성에 실패한 파일이 없습니다.",
            "No files failed to index."
        );
    }
    let mut out = tr!(
        "共 {total} 个文件建立索引失败，其中 {retryable} 个可以重试：",
        "파일 {total}개의 색인 생성에 실패했습니다. 그중 {retryable}개는 다시 시도할 수 있습니다:",
        "{total} files failed to index; {retryable} of them can be retried:"
    );
    for f in files {
        let _ = write!(out, "\n  {}", f.path);
        if !f.retryable {
            out.push_str(trs!(
                "（加密文档，不能重试）",
                " (암호화된 문서, 다시 시도 불가)",
                " (encrypted document, cannot be retried)"
            ));
        }
        let _ = write!(
            out,
            "\n    {}",
            crate::cli::plain(f.reason.as_deref().unwrap_or(trs!(
                "原因未知。",
                "원인을 알 수 없습니다.",
                "Unknown reason."
            )))
        );
    }
    if total > files.len() as u64 {
        let n = files.len();
        let _ = write!(
            out,
            "\n{}",
            tr!(
                "只列出了最近的 {n} 个。",
                "최근 {n}개만 표시했습니다.",
                "Only the {n} most recent are listed."
            )
        );
    }
    out
}

#[derive(Serialize)]
struct FailedOut<'a> {
    protocol: u32,
    total: u64,
    retryable: u64,
    files: &'a [FailedFile],
}

async fn params_of(s: &mut Session) -> Result<BTreeMap<String, ParamValue>, String> {
    let Reply::Params { values } = s.request(Request::GetParams).await? else {
        return Err(unexpected());
    };
    Ok(values)
}

async fn roots_of(s: &mut Session) -> Result<Vec<RootInfo>, String> {
    let Reply::Roots { roots } = s.request(Request::ListRoots).await? else {
        return Err(unexpected());
    };
    Ok(roots)
}

/// 已索引的文件夹；不是时的错误列出全部已索引的文件夹
async fn root_of(s: &mut Session, path: &Path) -> Result<RootInfo, String> {
    let roots = roots_of(s).await?;
    let path = absolute(path)?;
    find_root(&roots, &path)
        .cloned()
        .ok_or_else(|| super::output::not_a_root(Path::new(&path), &roots))
}

/// 把识别内容的类型改为 `kinds`，返回改后开启的类型
async fn set_kinds(s: &mut Session, kinds: Option<&[String]>) -> Result<Vec<&'static str>, String> {
    let values = params_of(s).await?;
    let Some(kinds) = kinds else {
        return Ok(enabled_kinds(&values));
    };
    for (key, value) in kind_changes(&values, kinds) {
        let body = Request::SetParam {
            key: key.into(),
            value,
        };
        s.request(body).await?;
    }
    Ok(CONTENT_KINDS
        .iter()
        .map(|c| c.0)
        .filter(|k| kinds.iter().any(|w| w == k))
        .collect())
}

fn print(text: &str) -> Result<(), String> {
    emit(text).map_err(|err| print_failed(&err))
}

pub(super) async fn run(s: &mut Session, cmd: Cmd, json: bool) -> Result<Outcome, String> {
    match cmd {
        Cmd::Folders => {
            let values = params_of(s).await?;
            let roots = roots_of(s).await?;
            // 索引尚未启动时只是没有进度
            let index = match s.request(Request::IndexStatus).await {
                Ok(Reply::Index(i)) => Some(i),
                _ => None,
            };
            let kinds = enabled_kinds(&values);
            print(&if json {
                ascii_json(&FoldersOut {
                    protocol: PROTOCOL_VERSION,
                    kinds: &kinds,
                    roots: &roots,
                    index: index.as_ref(),
                })
            } else {
                folders_text(&kinds, &roots, index.as_ref())
            })?;
            Ok(if roots.is_empty() {
                Outcome::Empty
            } else {
                Outcome::Found
            })
        }
        Cmd::AddFolder {
            path,
            kinds,
            excludes,
        } => {
            let body = Request::AddRoot {
                path: absolute(&path)?,
                excludes: excludes.unwrap_or_default(),
            };
            // 路径是后台服务规范化后的，搜索的 --folder 要用它
            let Reply::Root { root } = s.request(body).await? else {
                return Err(unexpected());
            };
            let kinds = set_kinds(s, kinds.as_deref()).await.map_err(|err| {
                tr!(
                    "文件夹已添加，但文件类型未能保存：{err}",
                    "폴더를 추가했지만 파일 유형을 저장하지 못했습니다: {err}",
                    "The folder was added, but the file types could not be saved: {err}"
                )
            })?;
            let mut msg = tr!(
                "已添加 {}，正在后台扫描并建立索引。",
                "{} 폴더를 추가했습니다. 백그라운드에서 스캔하고 색인을 만드는 중입니다.",
                "Added {}; scanning and indexing in the background.",
                root.path
            );
            let _ = write!(
                msg,
                "\n{}\n{}",
                rules_line(&root.excludes),
                kinds_line(&kinds_text(&kinds))
            );
            if whole_drive(&root.path) {
                let _ = write!(msg, "\n{}", whole_drive_note());
            }
            print(&if json {
                done_json(&msg, Some(&root))
            } else {
                msg
            })?;
            Ok(Outcome::Found)
        }
        Cmd::SetFolder {
            path,
            kinds,
            excludes,
        } => {
            let mut root = root_of(s, &path).await?;
            if let Some(excludes) = excludes {
                let body = Request::SetRootExcludes {
                    id: root.id,
                    excludes,
                    skip_nomedia: None,
                };
                s.request(body).await?;
                root = root_of(s, Path::new(&root.path)).await?;
            }
            let kinds = set_kinds(s, kinds.as_deref()).await?;
            let msg = format!(
                "{}\n{}\n{}",
                tr!(
                    "已更新 {}，将按新设置重新扫描。",
                    "{} 폴더를 업데이트했습니다. 새 설정으로 다시 스캔합니다.",
                    "Updated {}; it will be rescanned with the new settings.",
                    root.path
                ),
                rules_line(&root.excludes),
                kinds_line(&kinds_text(&kinds))
            );
            print(&if json {
                done_json(&msg, Some(&root))
            } else {
                msg
            })?;
            Ok(Outcome::Found)
        }
        Cmd::RemoveFolder(path) => {
            let root = root_of(s, &path).await?;
            s.request(Request::RemoveRoot { id: root.id }).await?;
            let msg = tr!(
                "已移除 {} 的索引，文件本身没有删除。",
                "{} 폴더의 색인을 제거했습니다. 파일은 지우지 않았습니다.",
                "Removed the index for {}; the files themselves were not deleted.",
                root.path
            );
            print(&if json { done_json(&msg, None) } else { msg })?;
            Ok(Outcome::Found)
        }
        Cmd::Settings(key) => {
            let values = params_of(s).await?;
            let defs: Vec<&ParamDef> = match &key {
                Some(k) => vec![params::def(k).ok_or_else(|| no_param(k))?],
                None => params::PARAMS.iter().filter(|d| d.shown()).collect(),
            };
            let text = if json {
                ascii_json(&SettingsOut {
                    protocol: PROTOCOL_VERSION,
                    settings: defs
                        .iter()
                        .map(|d| SettingOut {
                            def: d,
                            value: current(&values, d),
                        })
                        .collect(),
                })
            } else if key.is_some() {
                let mut out = String::new();
                setting_text(&mut out, defs[0], current(&values, defs[0]));
                out.trim_start().to_string()
            } else {
                settings_text(&values)
            };
            print(&text)?;
            Ok(Outcome::Found)
        }
        Cmd::SetSetting { key, value } => {
            let d = params::def(&key).ok_or_else(|| no_param(&key))?;
            let parsed = parse_value(d.kind, &value).ok_or_else(|| {
                tr!(
                    "“{}”的值应为{}。",
                    "‘{}’의 값은 {}여야 합니다.",
                    "The value of \"{}\" must be {}.",
                    d.label,
                    range_text(d.kind)
                )
            })?;
            let parsed = params::validate(&key, &parsed).map_err(|err| err.to_string())?;
            let before = params_of(s).await?;
            let changed = before.get(d.key) != Some(&parsed);
            let rebuild = rebuild_kinds(d.key, &before, parsed);
            let mut msg = set_text(d, parsed, changed, rebuild);
            if changed {
                let body = Request::SetParam {
                    key: key.clone(),
                    value: parsed,
                };
                if let Reply::DoneWithWarning { message } = s.request(body).await? {
                    let _ = write!(msg, "\n{}", crate::cli::plain(&message));
                }
            }
            print(&if json {
                ascii_json(&SetOut {
                    protocol: PROTOCOL_VERSION,
                    key: d.key,
                    value: parsed,
                    changed,
                    rebuild,
                    message: &msg,
                })
            } else {
                msg
            })?;
            Ok(Outcome::Found)
        }
        Cmd::Pause(minutes) => {
            let until = minutes.map(|m| Local::now().timestamp_millis() + i64::from(m) * 60_000);
            s.request(Request::PauseIndexing { until }).await?;
            let msg = match until.and_then(|t| Local.timestamp_millis_opt(t).single()) {
                Some(t) => tr!(
                    "已暂停建立索引，到 {} 自动恢复。暂停期间照常可以搜索。",
                    "색인 생성을 일시정지했습니다. {}에 자동으로 계속합니다. 일시정지 중에도 검색할 수 있습니다.",
                    "Indexing paused; it resumes automatically at {}. You can still search while paused.",
                    t.format("%Y-%m-%d %H:%M")
                ),
                None => tr!(
                    "已暂停建立索引，直到执行 resume 或在 Rummage 中恢复。暂停期间照常可以搜索。",
                    "색인 생성을 일시정지했습니다. resume을 실행하거나 Rummage에서 계속할 때까지 멈춥니다. 일시정지 중에도 검색할 수 있습니다.",
                    "Indexing paused until you run resume or resume it in Rummage. You can still search while paused."
                ),
            };
            print(&if json { done_json(&msg, None) } else { msg })?;
            Ok(Outcome::Found)
        }
        Cmd::Resume => {
            s.request(Request::ResumeIndexing).await?;
            let msg = trs!(
                "已恢复建立索引。未接电源、全屏或磁盘空间不足时仍会按设置自动暂停。",
                "색인 생성을 계속합니다. 전원이 연결되지 않았거나 전체 화면이거나 디스크 공간이 부족하면 설정에 따라 다시 자동으로 일시정지됩니다.",
                "Indexing resumed. It still pauses automatically, per your settings, on battery, in full screen or when disk space is low."
            );
            print(&if json {
                done_json(msg, None)
            } else {
                msg.into()
            })?;
            Ok(Outcome::Found)
        }
        Cmd::Failed => {
            let body = Request::FailedFiles {
                after: None,
                limit: FAILED_SHOWN,
            };
            let Reply::FailedFiles {
                files,
                total,
                retryable,
            } = s.request(body).await?
            else {
                return Err(unexpected());
            };
            print(&if json {
                ascii_json(&FailedOut {
                    protocol: PROTOCOL_VERSION,
                    total,
                    retryable,
                    files: &files,
                })
            } else {
                failed_text(&files, total, retryable)
            })?;
            Ok(if total == 0 {
                Outcome::Empty
            } else {
                Outcome::Found
            })
        }
        Cmd::RetryFailed => {
            let body = Request::FailedFiles {
                after: None,
                limit: 1,
            };
            let Reply::FailedFiles {
                total, retryable, ..
            } = s.request(body).await?
            else {
                return Err(unexpected());
            };
            if retryable > 0 {
                s.request(Request::RetryAllFailed).await?;
            }
            let mut msg = if retryable == 0 {
                tr!(
                    "没有可以重试的失败文件。",
                    "다시 시도할 수 있는 실패한 파일이 없습니다.",
                    "There are no failed files to retry."
                )
            } else {
                tr!(
                    "已将 {retryable} 个失败的文件重新排队建立索引。",
                    "실패한 파일 {retryable}개를 다시 색인 대기열에 넣었습니다.",
                    "Queued {retryable} failed files for indexing again."
                )
            };
            if total > retryable {
                let n = total - retryable;
                msg.push_str(&tr!(
                    "另有 {n} 个加密文档不能重试。",
                    " 암호화된 문서 {n}개는 다시 시도할 수 없습니다.",
                    " {n} encrypted documents cannot be retried."
                ));
            }
            print(&if json { done_json(&msg, None) } else { msg })?;
            Ok(if retryable == 0 {
                Outcome::Empty
            } else {
                Outcome::Found
            })
        }
        Cmd::Search(_) | Cmd::Status | Cmd::Context(_) => {
            unreachable!("search、status、context 由 cli::run 处理")
        }
    }
}
