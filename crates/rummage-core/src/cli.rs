//! 命令行子命令 `search`、`status`、`context`，以及查看与操作应用的 `folders`、`settings`、`pause`、`resume`、`failed`
//! （`manage`）：解析参数，连接核心（未运行时拉起，
//! `client::connect_or_start`），发出与界面相同的请求，再按 `output` 输出给人看的文本或 `--json`。
//! - 数据目录：`--data-dir` 指定（必须已有 `index.db`），否则用 `rummage_datadir::locate`（只解析、不迁移）。
//! - 每次运行新建一个 IPC 连接，用完即退出；搜索不计入搜索历史（不发 `AddHistory`）。
//! - `--image` 与 `--folder` 的相对路径按本进程的当前目录转成绝对路径（核心的当前目录不同）。
//!   `--folder` 可重复，每个是已索引的文件夹或其中的子文件夹，由核心定位（`LocateFolder`，不区分大小写），搜索范围取并集。
//! - 日期写成 `YYYY-MM-DD`，按本地时区，`--since` 从当天 0 点起，`--until` 到当天结束，都包含当天。
//! - 退出码：0 有结果或操作成功，1 没有结果（含命中已失效、命中没有文字、没有失败的文件），2 出错（用法错误、
//!   连不上核心、请求被拒绝）。
//! - 不记录日志：本进程不初始化日志，查询文本只出现在发给核心的请求中。
//! - 输出到 stdout 一律经过 `emit`（`emitln!`），读取方关闭管道时按正常结束处理，不像 `println!` 那样 panic。

/// 同 `println!`，但经过 `emit`；输出失败（读取方关闭管道以外的错误）时忽略，与 stdout 不可用时一样
macro_rules! emitln {
    () => {{
        let _ = $crate::cli::emit("");
    }};
    ($($arg:tt)*) => {{
        let _ = $crate::cli::emit(&format!($($arg)*));
    }};
}

pub(crate) mod export;
mod manage;
pub(crate) mod output;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{Days, Local, NaiveDate, TimeZone};
use rummage_core::client::{self, Client, ConnectError};
use rummage_proto::i18n::{Lang, set_lang};
use rummage_proto::{ApiError, ParamKind, Reply, Request, params, tr, trs};

fn bad_output() -> String {
    tr!(
        "--output 的文件扩展名应为 .csv、.json 或 .txt。",
        "--output 파일의 확장자는 .csv, .json, .txt 중 하나여야 합니다.",
        "The --output file extension must be .csv, .json or .txt."
    )
}

pub(crate) fn unknown_arg(arg: &str) -> String {
    tr!(
        "无法识别的参数：{arg}。",
        "알 수 없는 인수입니다: {arg}",
        "Unrecognized argument: {arg}"
    )
}

pub(crate) fn missing_dir() -> String {
    tr!(
        "--data-dir 后面缺少路径。",
        "--data-dir 뒤에 경로가 없습니다.",
        "--data-dir needs a path after it."
    )
}

pub(crate) fn no_runtime(err: &io::Error) -> String {
    tr!(
        "无法启动异步运行时：{err}",
        "비동기 런타임을 시작할 수 없습니다: {err}",
        "Could not start the async runtime: {err}"
    )
}

/// 命令行与 MCP 在自己的进程中运行，界面语言要自己定：先按环境变量 `LC_ALL` / `LANG`，都没有时为韩语；
/// 连上后台服务后改用外观设置中的语言（`lang_from_core`）
pub(crate) fn lang_from_env() {
    let v = ["LC_ALL", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default()
        .to_lowercase();
    set_lang(if v.starts_with("zh") {
        Lang::Zh
    } else if v.starts_with("en") {
        Lang::En
    } else {
        Lang::Ko
    });
}

/// 按外观设置中的语言改界面语言；没有设置或取不到时保持 `lang_from_env` 定的
pub(crate) async fn lang_from_core(client: &mut Client) {
    if let Ok(Ok(Reply::Appearance(a))) = client::request(client, 0, Request::GetAppearance).await
        && let Some(lang) = a.language
    {
        set_lang(lang);
    }
}

pub(crate) const KINDS: [&str; 5] = ["document", "image", "audio", "video", "other"];

/// 文件类型的英文名，核心的其他类型都记作 `other`（MCP 与导出共用）
pub(crate) fn kind_name(kind: &str) -> &str {
    match kind {
        "document" | "image" | "audio" | "video" => kind,
        _ => "other",
    }
}

/// 文件类型列表（命令行与 MCP 共用）：不区分大小写，跳过空项，去重；有不在 `allowed` 中的类型时返回这一项
pub(crate) fn parse_kinds<'a>(
    items: impl IntoIterator<Item = &'a str>,
    allowed: &[&str],
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for k in items {
        let k = k.trim().to_lowercase();
        if k.is_empty() {
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            return Err(k);
        }
        if !out.contains(&k) {
            out.push(k);
        }
    }
    Ok(out)
}

/// 参数 `result_limit` 的范围：上限即 `--limit` 的上限；核心不接受低于下限的数量，更少时按下限取回后再截断
pub(crate) fn limit_range() -> (u32, u32) {
    match params::def("result_limit").map(|d| d.kind) {
        Some(ParamKind::Int { min, max }) => (min as u32, max as u32),
        _ => unreachable!("result_limit 登记为整数参数"),
    }
}

pub fn usage() -> String {
    let max = limit_range().1;
    tr!(
        "用法：
  rummage-core search [<查询>] [--image <图片>] [--type <类型,…>] [--folder <文件夹>]
                      [--since <日期>] [--until <日期>] [--limit <数量>] [--include-weak]
                      [--output <文件>] [--json]
  rummage-core status [--json]
  rummage-core context <命中 ID> [--json]
  rummage-core folders [--json]                   已索引的文件夹、识别内容的文件类型与进度
  rummage-core folders add <文件夹> [--types <类型,…>] [--exclude <规则>]… [--json]
  rummage-core folders set <文件夹> [--types <类型,…>] [--exclude <规则>]… [--no-exclude] [--json]
  rummage-core folders remove <文件夹> [--json]   只移除索引，不删除文件
  rummage-core settings [<键>] [--json]           查看参数：当前值、默认值、范围、是否影响索引
  rummage-core settings set <键> <值> [--json]
  rummage-core pause [--minutes <分钟>] [--json]  暂停建立索引
  rummage-core resume [--json]
  rummage-core failed [--json]                    建立索引失败的文件与原因
  rummage-core failed retry [--json]              重试全部失败的文件
  rummage-core mcp                        以 stdio 方式运行 MCP 服务，供 AI 客户端调用

  按内容搜索已索引的文件。查询可以是一句描述，也可以是关键词；--image 以图搜图，可同时给出文字描述。
  类型：document、image、audio、video、other（只按文件名索引的文件），多个用逗号分隔。
  文件夹：已索引的文件夹或其中的子文件夹，可重复给出多个，已索引的文件夹可用 status 查看。日期：YYYY-MM-DD，本地时间，包含当天。数量：1–{max}，默认 20。
  --include-weak 同时显示匹配度较低的结果；--json 输出 JSON（非 ASCII 字符转义为 \\uXXXX）。
  --output 把结果保存到新文件，格式按扩展名：.csv、.json 或 .txt（每行一个路径）。
  命中 ID 见 search 的结果，context 输出该处已索引的正文与前后段落。
  folders 的 --types 为识别内容的文件类型（document、image、audio、video），所有文件夹共用这一设置；
  --exclude 每次给一条排除规则（写法同 .gitignore），可重复；folders set 给出 --exclude 时整体替换原有的规则，
  --no-exclude 清空。settings set 的值：开关写 true 或 false，有名称的选项可写名称。
  pause 的分钟数为 1–{pause}，不给时一直暂停到 resume；暂停期间照常可以搜索。
  以上命令都可以加 --data-dir <目录> 指定数据目录。
  退出码：0 有结果或操作成功，1 没有结果，2 出错。",
        "사용법:
  rummage-core search [<검색어>] [--image <이미지>] [--type <유형,…>] [--folder <폴더>]
                      [--since <날짜>] [--until <날짜>] [--limit <개수>] [--include-weak]
                      [--output <파일>] [--json]
  rummage-core status [--json]
  rummage-core context <결과 ID> [--json]
  rummage-core folders [--json]                   색인 폴더, 내용을 인식할 파일 유형, 진행 상황
  rummage-core folders add <폴더> [--types <유형,…>] [--exclude <규칙>]… [--json]
  rummage-core folders set <폴더> [--types <유형,…>] [--exclude <규칙>]… [--no-exclude] [--json]
  rummage-core folders remove <폴더> [--json]     색인만 제거하고 파일은 지우지 않음
  rummage-core settings [<키>] [--json]           설정 보기: 현재값, 기본값, 범위, 색인에 영향을 주는지
  rummage-core settings set <키> <값> [--json]
  rummage-core pause [--minutes <분>] [--json]    색인 생성 일시정지
  rummage-core resume [--json]
  rummage-core failed [--json]                    색인 생성에 실패한 파일과 원인
  rummage-core failed retry [--json]              실패한 파일 모두 다시 시도
  rummage-core mcp                        AI 클라이언트에서 쓸 MCP 서비스를 stdio로 실행

  색인된 파일을 내용으로 검색합니다. 검색어는 설명하는 문장이나 키워드 모두 됩니다. --image는 이미지로 검색하며, 텍스트 설명을 함께 줄 수 있습니다.
  유형: document, image, audio, video, other(파일 이름으로만 색인된 파일). 여러 개는 쉼표로 구분합니다.
  폴더: 색인 폴더나 그 하위 폴더이며, 여러 번 줄 수 있습니다. 색인 폴더는 status로 확인합니다. 날짜: YYYY-MM-DD, 현지 시간, 그날 포함. 개수: 1–{max}, 기본값 20.
  --include-weak는 일치도가 낮은 결과도 함께 표시합니다. --json은 JSON으로 출력합니다(ASCII가 아닌 문자는 \\uXXXX로 이스케이프).
  --output은 결과를 새 파일에 저장합니다. 형식은 확장자에 따라 .csv, .json, .txt(한 줄에 경로 하나) 중 하나입니다.
  결과 ID는 search 결과에 나오며, context는 그 위치의 색인된 본문과 앞뒤 단락을 출력합니다.
  folders의 --types는 내용을 인식할 파일 유형(document, image, audio, video)이며, 모든 폴더에 같이 적용됩니다.
  --exclude는 제외 규칙을 하나씩 지정하며(.gitignore와 같은 문법) 여러 번 줄 수 있습니다. folders set에서 --exclude를 주면 기존 규칙을 통째로 바꾸고,
  --no-exclude는 규칙을 모두 지웁니다. settings set의 값: 켜기/끄기는 true 또는 false, 이름이 있는 선택지는 이름으로 써도 됩니다.
  pause의 분은 1–{pause}이며, 생략하면 resume할 때까지 일시정지합니다. 일시정지 중에도 검색은 할 수 있습니다.
  위 명령에는 모두 --data-dir <폴더>를 붙여 데이터 폴더를 지정할 수 있습니다.
  종료 코드: 0 결과 있음 또는 작업 성공, 1 결과 없음, 2 오류.",
        "Usage:
  rummage-core search [<query>] [--image <image>] [--type <type,…>] [--folder <folder>]
                      [--since <date>] [--until <date>] [--limit <count>] [--include-weak]
                      [--output <file>] [--json]
  rummage-core status [--json]
  rummage-core context <hit ID> [--json]
  rummage-core folders [--json]                   Indexed folders, content file types and progress
  rummage-core folders add <folder> [--types <type,…>] [--exclude <rule>]… [--json]
  rummage-core folders set <folder> [--types <type,…>] [--exclude <rule>]… [--no-exclude] [--json]
  rummage-core folders remove <folder> [--json]   Removes only the index; files are not deleted
  rummage-core settings [<key>] [--json]          Show settings: current value, default, range, whether it affects the index
  rummage-core settings set <key> <value> [--json]
  rummage-core pause [--minutes <minutes>] [--json]  Pause indexing
  rummage-core resume [--json]
  rummage-core failed [--json]                    Files that failed to index, with reasons
  rummage-core failed retry [--json]              Retry all failed files
  rummage-core mcp                        Run the MCP server over stdio for AI clients

  Searches indexed files by content. The query can be a description or keywords; --image searches by image and can be combined with a text description.
  Types: document, image, audio, video, other (files indexed by name only), separated by commas.
  Folders: an indexed folder or a subfolder of one; can be given more than once. See indexed folders with status. Dates: YYYY-MM-DD, local time, inclusive. Count: 1–{max}, default 20.
  --include-weak also shows lower-match results; --json prints JSON (non-ASCII characters escaped as \\uXXXX).
  --output saves the results to a new file, in the format given by the extension: .csv, .json or .txt (one path per line).
  Hit IDs appear in search results; context prints the indexed text at that hit with the paragraphs before and after.
  --types for folders sets the file types whose content is recognized (document, image, audio, video); it applies to all folders.
  --exclude gives one exclude rule each time (.gitignore syntax) and can be repeated; with folders set, --exclude replaces the existing rules,
  --no-exclude clears them. Values for settings set: true or false for switches; named options can be given by name.
  pause takes 1–{pause} minutes; without it, indexing stays paused until resume. You can still search while paused.
  All commands above accept --data-dir <folder> to choose the data folder.
  Exit codes: 0 results found or success, 1 no results, 2 error.",
        pause = crate::manage::MAX_PAUSE_MIN
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchArgs {
    pub text: Option<String>,
    pub image: Option<PathBuf>,
    pub kinds: Vec<String>,
    /// 为空表示不限
    pub folders: Vec<PathBuf>,
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    pub limit: u32,
    pub include_weak: bool,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Search(SearchArgs),
    Status,
    Context(i64),
    Folders,
    /// `kinds`、`excludes` 为空表示不改（添加时排除规则为空）
    AddFolder {
        path: PathBuf,
        kinds: Option<Vec<String>>,
        excludes: Option<Vec<String>>,
    },
    SetFolder {
        path: PathBuf,
        kinds: Option<Vec<String>>,
        excludes: Option<Vec<String>>,
    },
    RemoveFolder(PathBuf),
    /// 为空时列出全部参数
    Settings(Option<String>),
    SetSetting {
        key: String,
        value: String,
    },
    /// 暂停的分钟数，为空时一直暂停到恢复
    Pause(Option<u32>),
    Resume,
    Failed,
    RetryFailed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub cmd: Cmd,
    pub json: bool,
    pub data_dir: Option<PathBuf>,
}

/// 运行的结果，对应退出码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Found,
    Empty,
    Failed,
}

impl From<Outcome> for ExitCode {
    fn from(o: Outcome) -> Self {
        ExitCode::from(match o {
            Outcome::Found => 0,
            Outcome::Empty => 1,
            Outcome::Failed => 2,
        })
    }
}

fn utf8(v: OsString, flag: &str) -> Result<String, String> {
    v.into_string().map_err(|_| {
        tr!(
            "{flag} 的值含有无法识别的字符。",
            "{flag}의 값에 인식할 수 없는 문자가 있습니다.",
            "The value of {flag} contains unrecognized characters."
        )
    })
}

pub(crate) fn parse_date(s: &str, flag: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| {
            tr!(
                "{flag} 的日期“{s}”无法识别，请写成 YYYY-MM-DD，例如 2026-01-05。",
                "{flag} 날짜를 인식할 수 없습니다: ‘{s}’. YYYY-MM-DD 형식으로 쓰십시오(예: 2026-01-05).",
                "Could not read the {flag} date \"{s}\". Write it as YYYY-MM-DD, e.g. 2026-01-05."
            )
        })
}

pub(crate) fn missing_value(flag: &str) -> String {
    tr!(
        "{flag} 后面缺少值。",
        "{flag} 뒤에 값이 없습니다.",
        "{flag} needs a value after it."
    )
}

/// 类型不在 `allowed` 中（命令行的 `--type` 与 `--types` 共用）
pub(crate) fn unknown_kind(kind: &str, allowed: &[&str]) -> String {
    tr!(
        "无法识别的类型“{kind}”，可选：{}。",
        "알 수 없는 유형입니다: ‘{kind}’. 사용할 수 있는 유형: {}",
        "Unrecognized type \"{kind}\". Choose from: {}.",
        allowed.join(trs!("、", ", ", ", "))
    )
}

pub(crate) fn bad_path(p: &Path) -> String {
    tr!(
        "无法识别路径 \"{}\"。",
        "경로를 인식할 수 없습니다: \"{}\"",
        "Could not read the path \"{}\".",
        p.display()
    )
}

pub(crate) fn print_failed(err: &io::Error) -> String {
    tr!(
        "无法输出结果：{err}",
        "결과를 출력할 수 없습니다: {err}",
        "Could not print the results: {err}"
    )
}

/// 子命令名（`main` 据此分派给 `parse`）
pub const COMMANDS: [&str; 8] = [
    "search", "status", "context", "folders", "settings", "pause", "resume", "failed",
];

/// `cmd` 为 `COMMANDS` 之一，`args` 为其后的参数
pub fn parse(cmd: &str, args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    if !matches!(cmd, "search" | "status" | "context") {
        return manage::parse(cmd, args);
    }
    let mut args = args.into_iter();
    let mut json = false;
    let mut data_dir = None;
    let mut words = Vec::new();
    let mut s = SearchArgs {
        text: None,
        image: None,
        kinds: Vec::new(),
        folders: Vec::new(),
        since: None,
        until: None,
        limit: 20,
        include_weak: false,
        output: None,
    };
    while let Some(arg) = args.next() {
        let flag = arg.to_string_lossy().into_owned();
        if !flag.starts_with("--") {
            words.push(utf8(arg, trs!("查询", "검색어", "query"))?);
            continue;
        }
        match flag.as_str() {
            "--json" => {
                json = true;
                continue;
            }
            "--include-weak" if cmd == "search" => {
                s.include_weak = true;
                continue;
            }
            _ => {}
        }
        let known = matches!(flag.as_str(), "--data-dir")
            || (cmd == "search"
                && matches!(
                    flag.as_str(),
                    "--image"
                        | "--type"
                        | "--folder"
                        | "--since"
                        | "--until"
                        | "--limit"
                        | "--output"
                ));
        if !known {
            return Err(unknown_arg(&flag));
        }
        let value = args.next().ok_or_else(|| missing_value(&flag))?;
        match flag.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value)),
            "--image" => s.image = Some(PathBuf::from(value)),
            "--folder" => s.folders.push(PathBuf::from(value)),
            "--output" => {
                let path = PathBuf::from(value);
                if export::ext(&path).is_none() {
                    return Err(bad_output());
                }
                s.output = Some(path);
            }
            "--type" => {
                for k in parse_kinds(utf8(value, &flag)?.split([',', '，']), &KINDS)
                    .map_err(|k| unknown_kind(&k, &KINDS))?
                {
                    if !s.kinds.contains(&k) {
                        s.kinds.push(k);
                    }
                }
            }
            "--since" => s.since = Some(parse_date(&utf8(value, &flag)?, &flag)?),
            "--until" => s.until = Some(parse_date(&utf8(value, &flag)?, &flag)?),
            _ => {
                let max = limit_range().1;
                s.limit = utf8(value, &flag)?
                    .parse()
                    .ok()
                    .filter(|n| (1..=max).contains(n))
                    .ok_or_else(|| {
                        tr!(
                            "--limit 应为 1–{max} 的整数。",
                            "--limit는 1–{max} 사이의 정수여야 합니다.",
                            "--limit must be a whole number from 1 to {max}."
                        )
                    })?;
            }
        }
    }
    let cmd = match cmd {
        "search" => {
            let text = words.join(" ").trim().to_string();
            s.text = (!text.is_empty()).then_some(text);
            if s.text.is_none() && s.image.is_none() {
                return Err(tr!(
                    "请给出查询文字或 --image。",
                    "검색어나 --image를 지정하십시오.",
                    "Give a query or --image."
                ));
            }
            if let (Some(a), Some(b)) = (s.since, s.until)
                && a > b
            {
                return Err(tr!(
                    "--since 不能晚于 --until。",
                    "--since는 --until보다 늦을 수 없습니다.",
                    "--since cannot be later than --until."
                ));
            }
            Cmd::Search(s)
        }
        "status" if words.is_empty() => Cmd::Status,
        "status" => return Err(unknown_arg(&words[0])),
        _ => {
            let [id]: [String; 1] = words.try_into().map_err(|_| {
                tr!(
                    "context 需要且只需要一个命中 ID。",
                    "context에는 결과 ID를 하나만 지정해야 합니다.",
                    "context needs exactly one hit ID."
                )
            })?;
            let id = id.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
                tr!(
                    "命中 ID“{id}”应为正整数，见 search 的结果。",
                    "결과 ID는 양의 정수여야 합니다: ‘{id}’. search 결과를 확인하십시오.",
                    "The hit ID \"{id}\" must be a positive whole number; see the search results."
                )
            })?;
            Cmd::Context(id)
        }
    };
    Ok(Args {
        cmd,
        json,
        data_dir,
    })
}

/// 本地时区中 `since` 当天 0 点、`until` 次日 0 点前 1 毫秒的 Unix 毫秒
pub fn day_range<Tz: TimeZone>(
    tz: &Tz,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
) -> (Option<i64>, Option<i64>) {
    let start = |d: NaiveDate| {
        let at = |h| {
            let t = d.and_hms_opt(h, 0, 0).expect("0 点与 1 点总是有效的时刻");
            tz.from_local_datetime(&t).earliest()
        };
        // 夏令时从 0 点起跳（当天没有 0 点）时改用 1 点；两者都不存在只可能是时区数据有误，按 UTC 算
        at(0).or_else(|| at(1)).map_or_else(
            || {
                d.and_hms_opt(0, 0, 0)
                    .expect("0 点总是有效的时刻")
                    .and_utc()
                    .timestamp_millis()
            },
            |t| t.timestamp_millis(),
        )
    };
    let end = |d: NaiveDate| {
        d.checked_add_days(Days::new(1))
            .map_or(i64::MAX, |next| start(next) - 1)
    };
    (since.map(start), until.map(end))
}

/// 统一的“规范”形式：反斜杠、去掉末尾分隔符、小写，用于比较文件夹路径
pub(crate) fn folded(path: &str) -> String {
    path.replace('/', r"\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// 写到 stdout；读取方已关闭管道（如接 `Select-Object -First 1`）时按正常结束处理
pub fn emit(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        res => res,
    }
}

struct Session {
    client: Client,
    next: u64,
}

impl Session {
    async fn request(&mut self, body: Request) -> Result<Reply, String> {
        self.next += 1;
        match client::request(&mut self.client, self.next, body).await {
            Ok(Ok(reply)) => Ok(reply),
            // 核心的整句给界面用，界面中的位置改为命令行的写法（`plain`）
            Ok(Err(err)) => Err(plain(&err.message)),
            // 断开时的消息本身就是整句，不再加前缀
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Err(err.to_string()),
            Err(err) => Err(tr!(
                "与 Rummage 后台服务的连接中断：{err}",
                "Rummage 백그라운드 서비스와의 연결이 끊어졌습니다: {err}",
                "The connection to the Rummage background service was lost: {err}"
            )),
        }
    }
}

/// 后台服务的整句是给界面的：其中界面写法的「设置 → X」改为命令行与 MCP 的写法“Rummage 的 设置 → X”，
/// 与中文之间用空格隔开（前面是开头或空白、后面是标点或结尾时不加）。设备名等其他「」不动
pub(crate) fn plain(msg: &str) -> String {
    const OPEN: &str = "「设置 → ";
    let mut out = String::new();
    let mut rest = msg;
    while let Some(i) = rest.find(OPEN) {
        let Some(end) = rest[i..].find('」').map(|e| i + e) else {
            break;
        };
        let head = &rest[..i];
        out.push_str(head);
        if !head.is_empty() && !head.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        out.push_str("Rummage 的 ");
        out.push_str(&rest[i + "「".len()..end]);
        rest = &rest[end + "」".len()..];
        if rest
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && !"，。；：！？、）".contains(c))
        {
            out.push(' ');
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn unexpected() -> String {
    tr!(
        "Rummage 后台服务的回复无法识别，请确认命令行与 Rummage 是同一版本。",
        "Rummage 백그라운드 서비스의 응답을 인식할 수 없습니다. 명령줄 도구와 Rummage가 같은 버전인지 확인하십시오.",
        "The reply from the Rummage background service was not recognized. Make sure the command-line tool and Rummage are the same version."
    )
}

pub(crate) fn connect_error(err: ConnectError) -> String {
    match err {
        ConnectError::NotRunning => tr!(
            "Rummage 后台服务没有启动成功，请打开 Rummage 查看。",
            "Rummage 백그라운드 서비스를 시작하지 못했습니다. Rummage를 열어 확인하십시오.",
            "The Rummage background service did not start. Open Rummage to check."
        ),
        ConnectError::Rejected(ApiError { message, .. }) => plain(&message),
        err => err.to_string(),
    }
}

async fn run_search(s: &mut Session, a: SearchArgs, json: bool) -> Result<Outcome, String> {
    let cwd = |p: &Path| std::path::absolute(p).map_err(|_| bad_path(p));
    let mut folders = Vec::new();
    for folder in &a.folders {
        let folder = cwd(folder)?;
        let path = folder.to_string_lossy().into_owned();
        let Reply::FolderLocation { scope, .. } = s.request(Request::LocateFolder { path }).await?
        else {
            return Err(unexpected());
        };
        let Some(scope) = scope else {
            let Reply::Roots { roots } = s.request(Request::ListRoots).await? else {
                return Err(unexpected());
            };
            return Err(output::not_in_root(&folder, &roots));
        };
        folders.push(scope);
    }
    let image_path = match &a.image {
        Some(p) => Some(cwd(p)?.to_string_lossy().into_owned()),
        None => None,
    };
    let (since, until) = day_range(&Local, a.since, a.until);
    let body = Request::Search {
        text: a.text.clone(),
        image_path,
        similar: None,
        kinds: a.kinds.clone(),
        folders,
        since,
        until,
        limit: Some(a.limit.max(limit_range().0)),
        snippets: true,
        supersede: false,
    };
    let Reply::Results {
        hits,
        took_ms,
        partial,
        semantic_unavailable,
        unavailable,
    } = s.request(body).await?
    else {
        return Err(unexpected());
    };
    let weak = if a.include_weak {
        0
    } else {
        hits.iter().filter(|h| h.low_relevance).count()
    };
    let shown: Vec<_> = hits
        .into_iter()
        .filter(|h| a.include_weak || !h.low_relevance)
        .take(a.limit as usize)
        .collect();
    let res = output::Results {
        hits: &shown,
        took_ms,
        partial,
        semantic_unavailable,
        unavailable,
        weak_omitted: weak,
        image: a.image.is_some(),
    };
    let text = match &a.output {
        Some(path) => {
            let path = cwd(path)?;
            export::save(&path, &shown).map_err(|err| match err {
                export::ExportError::Exists => {
                    tr!(
                        "\"{}\" 已存在，请换一个文件名。",
                        "\"{}\" 파일이 이미 있습니다. 다른 파일 이름을 쓰십시오.",
                        "\"{}\" already exists. Use a different file name.",
                        path.display()
                    )
                }
                export::ExportError::Format => bad_output(),
                export::ExportError::Io(err) => {
                    tr!(
                        "无法写入 \"{}\"：{err}",
                        "\"{}\"에 쓸 수 없습니다: {err}",
                        "Could not write \"{}\": {err}",
                        path.display()
                    )
                }
            })?;
            output::export_text(&res, &path, json)
        }
        None if json => output::search_json(&res),
        None => output::search_text(&res),
    };
    emit(&text).map_err(|err| print_failed(&err))?;
    Ok(if shown.is_empty() {
        Outcome::Empty
    } else {
        Outcome::Found
    })
}

async fn run_status(s: &mut Session, json: bool) -> Result<Outcome, String> {
    let Reply::Index(index) = s.request(Request::IndexStatus).await? else {
        return Err(unexpected());
    };
    let Reply::Roots { roots } = s.request(Request::ListRoots).await? else {
        return Err(unexpected());
    };
    let Reply::Setup(setup) = s.request(Request::SetupStatus).await? else {
        return Err(unexpected());
    };
    let st = output::Status {
        core_version: &s.client.welcome.core_version,
        onboarded: setup.onboarded,
        index: &index,
        roots: &roots,
    };
    let text = if json {
        output::status_json(&st)
    } else {
        output::status_text(&st)
    };
    emit(&text).map_err(|err| print_failed(&err))?;
    Ok(Outcome::Found)
}

async fn run_context(s: &mut Session, id: i64, json: bool) -> Result<Outcome, String> {
    let Reply::Segment { segment } = s.request(Request::SegmentText { segment_id: id }).await?
    else {
        return Err(unexpected());
    };
    let Some(seg) = segment else {
        eprintln!(
            "{}",
            tr!(
                "命中 {id} 已失效（文件可能已重新建立索引），请重新搜索。",
                "결과 ID {id}: 더 이상 유효하지 않습니다(파일이 다시 색인되었을 수 있음). 다시 검색하십시오.",
                "Hit {id} is no longer valid (the file may have been re-indexed). Search again."
            )
        );
        return Ok(Outcome::Empty);
    };
    if seg.text.is_none() {
        eprintln!(
            "{}",
            tr!(
                "命中 {id} 没有文字内容（图片、音视频，或文档中的图片或音视频）。",
                "결과 ID {id}: 텍스트 내용이 없습니다(이미지, 오디오·동영상 또는 문서 안의 이미지나 오디오·동영상).",
                "Hit {id} has no text (an image, audio or video, or one inside a document)."
            )
        );
        return Ok(Outcome::Empty);
    }
    // 文件已移除等情况下取不到信息，只输出正文
    let file = match s
        .request(Request::FileInfo {
            file_id: seg.file_id,
        })
        .await
    {
        Ok(Reply::File(info)) => Some(info),
        _ => None,
    };
    let text = if json {
        output::context_json(file.as_ref(), &seg)
    } else {
        output::context_text(file.as_ref(), &seg)
    };
    emit(&text).map_err(|err| print_failed(&err))?;
    Ok(Outcome::Found)
}

/// 要连接的数据目录：`--data-dir` 指定的（必须已有索引数据库），否则为当前指向的数据目录（只解析、不迁移）。MCP 共用
pub(crate) fn data_dir(dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let dir = match dir {
        // 指定的目录必须已有索引数据库，否则拉起的核心会在那里新建一个空的数据目录；相对路径按本进程的当前目录
        Some(dir) if dir.join("index.db").is_file() => {
            std::path::absolute(&dir).map_err(|err| {
                tr!(
                    "无法识别路径 \"{}\"：{err}",
                    "경로를 인식할 수 없습니다(\"{}\"): {err}",
                    "Could not read the path \"{}\": {err}",
                    dir.display()
                )
            })?
        }
        Some(dir) => {
            return Err(tr!(
                "这不是 Rummage 的数据目录（\"{}\"）。",
                "Rummage 데이터 폴더가 아닙니다(\"{}\").",
                "This is not a Rummage data folder (\"{}\").",
                dir.display()
            ));
        }
        None => rummage_datadir::locate().map_err(|err| match err {
            rummage_datadir::Error::Missing(_) | rummage_datadir::Error::Invalid { .. } => {
                tr!(
                    "{err}请先打开 Rummage。",
                    "{err} 먼저 Rummage를 실행하십시오.",
                    "{err} Open Rummage first."
                )
            }
            err => err.to_string(),
        })?,
    };
    Ok(dir)
}

pub async fn run(args: Args) -> Outcome {
    let res = async {
        let dir = data_dir(args.data_dir)?;
        let exe = std::env::current_exe().map_err(|err| {
            tr!(
                "无法确定 rummage-core 的位置：{err}",
                "rummage-core의 위치를 확인할 수 없습니다: {err}",
                "Could not determine the location of rummage-core: {err}"
            )
        })?;
        let mut client = client::connect_or_start(&dir, &exe, &mut None)
            .await
            .map_err(connect_error)?;
        lang_from_core(&mut client).await;
        let mut s = Session { client, next: 0 };
        match args.cmd {
            Cmd::Search(a) => run_search(&mut s, a, args.json).await,
            Cmd::Status => run_status(&mut s, args.json).await,
            Cmd::Context(id) => run_context(&mut s, id, args.json).await,
            cmd => manage::run(&mut s, cmd, args.json).await,
        }
    }
    .await;
    res.unwrap_or_else(|msg| {
        eprintln!("{msg}");
        Outcome::Failed
    })
}
