//! 模型文件的下载、导入与校验。
//! - 下载源：Hugging Face（默认）、魔搭或用户填写的与 Hugging Face 兼容的地址；地址钉定仓库提交
//!   （`resolve/<提交>/<文件名>`），仓库、提交与已知哈希表都在 `rummage_embed::space` 与 `npu`。每个文件带自己的仓库与提交
//!   （`Pinned`）：Android 的 LiteRT 模型与分词器在两个仓库中。魔搭上的仓库同名，提交号另记。
//! - 高通 NPU 的 QNN 运行库（`rummage_embed::npu`）从 Maven Central 的 AAR 中取：不下载整个包，按记下的位置用 HTTP Range
//!   先取条目的本地文件头（得到文件名与扩展字段的长度），再取压缩数据，解压（deflate）到 `.part`，核对大小与 SHA-256 后改名。
//!   不走 Hugging Face 的下载源，同样跟随代理设置；文件小（每个不超过约 5 MB），不续传。
//! - 下载前读取钉定提交的文件列表（Hugging Face 为 `/api/models/<仓库>/tree/<提交>` 的 `lfs.oid`，魔搭为
//!   `/api/v1/models/<仓库>/repo/files` 的 `Sha256`）取文件大小与 SHA-256，与已知表核对，不一致时拒绝
//!   （下载源可能不同步或内容被替换，向量空间也按已知哈希计算）；接口不可用（返回错误状态或内容无法解析）时直接用已知表。
//!   连不上下载源时直接报错，不再请求文件。
//! - 已有的文件先核对大小与 SHA-256，正确的跳过，全部正确时不联网（开发版数据目录中已有的模型不会被重新下载或覆盖）。
//! - 断点续传：写入同目录的 `<文件名>.part`；续传前先对已下载的部分计算哈希，再用 HTTP `Range` 请求剩下的部分，
//!   服务器不支持 Range（返回 200），或 206 回复的 Content-Range 起点与已下载的长度不符时，删掉 `.part` 从头下载。完成后核对大小与 SHA-256，通过才改名为正式文件（同一目录内改名，
//!   原子替换），不通过时删除 `.part` 并报错。取消或网络中断时保留 `.part`，下次续传。
//! - 导入：按大小找出对应的模型文件，边复制到 `<文件名>.import.part`（不碰下载用的 `.part`）边计算哈希，
//!   与已知哈希一致才改名；不认识的文件报错。
//! - 校验（`verify`）：对已安装的文件完整核对大小与 SHA-256，结果由调用方记下。
//! - 删除（`remove`）：删除模型文件与下载、导入留下的临时文件；LiteRT 为它生成的缓存另由 `remove_caches` 删除。调用方先让推理进程退出；Windows 上文件仍被其他程序
//!   打开时删不掉，提示稍后重试，不做强制删除。
//! - 给用户看的错误都是带处理建议的中文整句，英文的系统与网络错误只写进日志。
//! - 读取文件内容前都先检查网盘占位文件属性（`rummage_index::ensure_local`）。
//! - 网络错误转换成中文原因，能区分是不是代理服务器的问题。
//!
//! 任务在调用方的线程上运行（核心为每个任务开专用线程）；下载用一个单线程 tokio 运行时，读取与取消同时等待。
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use rummage_embed::npu::{self, QnnEntry};
use rummage_embed::space::{self, ModelFile};
use rummage_proto::{ModelSource, ModelTaskPhase, tr, trs};
use sha2::{Digest, Sha256};

use crate::proxy::Route;

pub const OFFICIAL: &str = "https://huggingface.co";
pub const MODELSCOPE: &str = "https://modelscope.cn";
const USER_AGENT: &str = concat!("rummage/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 读取响应时超过这么久没有收到数据就判为超时
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// 进度回调的最小间隔
const REPORT_EVERY: Duration = Duration::from_millis(250);
/// 取消标志的检查间隔
const CANCEL_POLL: Duration = Duration::from_millis(200);
const BUF: usize = 1 << 20;

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("{}", trs!("已取消。", "취소되었습니다.", "Cancelled."))]
    Cancelled,
    /// 给用户看的中文整句
    #[error("{0}")]
    Failed(String),
}

/// 钉定在某个仓库提交上的模型文件
#[derive(Debug, Clone, Copy)]
pub struct Pinned {
    pub file: &'static ModelFile,
    pub repo: &'static str,
    pub commit: &'static str,
    /// 魔搭上同名仓库的提交
    pub ms_commit: &'static str,
}

impl Pinned {
    /// 电脑的 GGUF 模型（`space::REPO`）
    pub const fn gguf(file: &'static ModelFile) -> Self {
        Pinned {
            file,
            repo: space::REPO,
            commit: space::COMMIT,
            ms_commit: space::MODELSCOPE_COMMIT,
        }
    }

    /// LiteRT 的通用版与 NPU 版模型（`space::LITERT_REPO`）
    pub const fn litert(file: &'static ModelFile) -> Self {
        Pinned {
            file,
            repo: space::LITERT_REPO,
            commit: space::LITERT_COMMIT,
            ms_commit: space::LITERT_MODELSCOPE_COMMIT,
        }
    }

    /// LiteRT 数切片长度用的分词器（`space::TOKENIZER_REPO`）
    pub const TOKENIZER: Pinned = Pinned {
        file: &space::TOKENIZER,
        repo: space::TOKENIZER_REPO,
        commit: space::TOKENIZER_COMMIT,
        ms_commit: space::TOKENIZER_MODELSCOPE_COMMIT,
    };

    /// 钉定版本的文件页面
    pub fn page(&self, base: &str) -> String {
        format!(
            "{base}/{}/blob/{}/{}",
            self.repo, self.commit, self.file.name
        )
    }
}

/// 当前文件的进度
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub phase: ModelTaskPhase,
    pub file: &'static str,
    /// 第几个文件（从 1 开始）与共几个
    pub index: u32,
    pub count: u32,
    pub done: u64,
    pub total: u64,
    /// 下载速度（字节每秒）
    pub speed: Option<u64>,
}

/// 安装进程默认的 ring 加密实现；已安装时什么也不做
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// 解析后的下载源，带基地址（不带末尾的 `/`）
#[derive(Debug, Clone, PartialEq)]
pub enum Hub {
    /// Hugging Face 或接口兼容的地址
    HuggingFace(String),
    ModelScope(String),
}

impl Hub {
    fn file_url(&self, f: &Pinned) -> String {
        match self {
            Hub::HuggingFace(base) => {
                format!("{base}/{}/resolve/{}/{}", f.repo, f.commit, f.file.name)
            }
            Hub::ModelScope(base) => format!(
                "{base}/models/{}/resolve/{}/{}",
                f.repo, f.ms_commit, f.file.name
            ),
        }
    }

    fn list_url(&self, f: &Pinned) -> String {
        match self {
            Hub::HuggingFace(base) => format!("{base}/api/models/{}/tree/{}", f.repo, f.commit),
            Hub::ModelScope(base) => format!(
                "{base}/api/v1/models/{}/repo/files?Revision={}&Recursive=true",
                f.repo, f.ms_commit
            ),
        }
    }
}

/// 自定义地址可以带路径前缀，但必须是 http 或 https，不能带查询参数
pub fn hub(source: &ModelSource) -> Result<Hub, String> {
    let url = match source {
        ModelSource::Official => return Ok(Hub::HuggingFace(OFFICIAL.into())),
        ModelSource::ModelScope => return Ok(Hub::ModelScope(MODELSCOPE.into())),
        ModelSource::Custom { url } => url.trim().trim_end_matches('/'),
    };
    let hint = trs!(
        "例如 https://hf-mirror.com",
        "예: https://hf-mirror.com",
        "e.g. https://hf-mirror.com"
    );
    if url.is_empty() {
        return Err(tr!(
            "请填写下载源地址，{hint}。",
            "다운로드 소스 주소를 입력하십시오({hint}).",
            "Enter a download source address ({hint})."
        ));
    }
    let parsed = Url::parse(url).map_err(|_| {
        tr!(
            "下载源地址格式不正确，{hint}。",
            "다운로드 소스 주소 형식이 올바르지 않습니다({hint}).",
            "The download source address is invalid ({hint})."
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(tr!(
            "下载源地址需以 http:// 或 https:// 开头，{hint}。",
            "다운로드 소스 주소는 http:// 또는 https://로 시작해야 합니다({hint}).",
            "The download source address must start with http:// or https:// ({hint})."
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(tr!(
            "下载源地址不能带查询参数，{hint}。",
            "다운로드 소스 주소에는 쿼리 매개변수를 넣을 수 없습니다({hint}).",
            "The download source address can't contain query parameters ({hint})."
        ));
    }
    Ok(Hub::HuggingFace(url.to_string()))
}

/// 节流后交给回调的进度
struct Reporter<'a> {
    cb: &'a mut dyn FnMut(&Progress),
    p: Progress,
    last: Instant,
    /// 计算速度的起点：时刻与当时已处理的字节数
    since: (Instant, u64),
}

impl<'a> Reporter<'a> {
    fn new(cb: &'a mut dyn FnMut(&Progress), count: usize) -> Self {
        let now = Instant::now();
        Reporter {
            cb,
            p: Progress {
                phase: ModelTaskPhase::Preparing,
                file: "",
                index: 0,
                count: count as u32,
                done: 0,
                total: 0,
                speed: None,
            },
            last: now,
            since: (now, 0),
        }
    }

    /// 进入新的一步，立即回调；`index` 从 0 开始
    fn start(&mut self, phase: ModelTaskPhase, file: &ModelFile, index: usize, done: u64) {
        let now = Instant::now();
        self.p = Progress {
            phase,
            file: file.name,
            index: index as u32 + 1,
            count: self.p.count,
            done,
            total: file.size,
            speed: None,
        };
        self.since = (now, done);
        self.last = now;
        (self.cb)(&self.p);
    }

    fn advance(&mut self, done: u64) {
        self.p.done = done;
        let now = Instant::now();
        if now.duration_since(self.last) < REPORT_EVERY {
            return;
        }
        self.last = now;
        if self.p.phase == ModelTaskPhase::Downloading {
            let secs = now.duration_since(self.since.0).as_secs_f64();
            self.p.speed = Some(((done - self.since.1) as f64 / secs.max(0.001)) as u64);
        }
        (self.cb)(&self.p);
    }
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_error(path: &Path, err: io::Error) -> Error {
    log::warn!("无法读取 {}：{err}", path.display());
    Error::Failed(tr!(
        "无法读取 \"{}\"，请确认文件存在、没有被其他程序占用后重试。",
        "\"{}\" 파일을 읽을 수 없습니다. 파일이 있는지, 다른 프로그램이 사용 중이지 않은지 확인한 뒤 다시 시도하십시오.",
        "Can't read \"{}\". Make sure the file exists and isn't in use by another program, then try again.",
        path.display()
    ))
}

fn write_error(err: io::Error) -> Error {
    log::warn!("无法写入模型文件：{err}");
    if err.kind() == ErrorKind::StorageFull {
        return Error::Failed(tr!(
            "磁盘空间不足，无法保存模型文件，请清理数据目录所在的磁盘后重试。",
            "디스크 공간이 부족해 모델 파일을 저장할 수 없습니다. 데이터 폴더가 있는 디스크의 공간을 확보한 뒤 다시 시도하십시오.",
            "Not enough disk space to save the model file. Free up space on the disk that holds the data folder, then try again."
        ));
    }
    Error::Failed(tr!(
        "无法保存模型文件，请检查数据目录所在磁盘的剩余空间与权限后重试。",
        "모델 파일을 저장할 수 없습니다. 데이터 폴더가 있는 디스크의 남은 공간과 권한을 확인한 뒤 다시 시도하십시오.",
        "Can't save the model file. Check the free space and permissions of the disk that holds the data folder, then try again."
    ))
}

/// 改名为正式文件失败。Windows 上目标被占用（ERROR_ACCESS_DENIED、ERROR_SHARING_VIOLATION，例如推理进程正加载着
/// 旧文件）时提示退出后重试
fn rename_error(err: io::Error) -> Error {
    if cfg!(windows) && matches!(err.raw_os_error(), Some(5 | 32)) {
        log::warn!("无法替换模型文件，文件正被使用：{err}");
        return Error::Failed(tr!(
            "模型文件正被使用，请从托盘退出 Rummage 后重试。",
            "모델 파일이 사용 중입니다. 트레이에서 Rummage를 종료한 뒤 다시 시도하십시오.",
            "The model file is in use. Quit Rummage from the tray, then try again."
        ));
    }
    write_error(err)
}

/// 计算整个文件的 SHA-256，返回哈希状态（续传时接着用）与读到的字节数；读取前检查占位文件属性
fn hash_file(path: &Path, cancel: &AtomicBool, rep: &mut Reporter) -> Result<(Sha256, u64), Error> {
    rummage_index::ensure_local(path).map_err(|err| Error::Failed(err.to_string()))?;
    let mut f = File::open(path).map_err(|err| read_error(path, err))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF];
    let mut n = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let len = f.read(&mut buf).map_err(|err| read_error(path, err))?;
        if len == 0 {
            return Ok((hasher, n));
        }
        hasher.update(&buf[..len]);
        n += len as u64;
        rep.advance(n);
    }
}

/// `path` 是完整、正确的模型文件；不存在或大小不对时不读内容
fn valid(
    path: &Path,
    file: &ModelFile,
    cancel: &AtomicBool,
    rep: &mut Reporter,
) -> Result<bool, Error> {
    match fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() == file.size => {}
        Ok(_) => return Ok(false),
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(read_error(path, err)),
    }
    let (hasher, _) = hash_file(path, cancel, rep)?;
    Ok(hex(&hasher.finalize()) == file.sha256)
}

/// 等到取消标志被设置
async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// 下载 `files` 到 `dir`（`<数据目录>\models\<子目录>`），再从 QNN 包中取出 `qnn` 到 `qnn_dir`（见模块说明），
/// 已正确的文件跳过
#[allow(clippy::too_many_arguments)]
pub fn download(
    hub: &Hub,
    files: &[Pinned],
    dir: &Path,
    qnn: &[&'static QnnEntry],
    qnn_dir: &Path,
    route: &Route,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Progress),
) -> Result<(), Error> {
    let mut rep = Reporter::new(progress, files.len() + qnn.len());
    let mut todo = Vec::new();
    for (i, f) in files.iter().enumerate() {
        rep.start(ModelTaskPhase::Checking, f.file, i, 0);
        if !valid(&dir.join(f.file.name), f.file, cancel, &mut rep)? {
            todo.push((i, *f));
        }
    }
    let mut qnn_todo = Vec::new();
    for (i, e) in qnn.iter().enumerate() {
        rep.start(ModelTaskPhase::Checking, &e.file, files.len() + i, 0);
        if !valid(&qnn_dir.join(e.file.name), &e.file, cancel, &mut rep)? {
            qnn_todo.push((files.len() + i, *e));
        }
    }
    if todo.is_empty() && qnn_todo.is_empty() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            log::error!("无法创建下载用的运行时：{err}");
            Error::Failed(tr!(
                "无法开始下载，请重试；仍然失败时请重新启动 Rummage。",
                "다운로드를 시작할 수 없습니다. 다시 시도하고, 계속 실패하면 Rummage를 다시 시작하십시오.",
                "Can't start the download. Try again; if it keeps failing, restart Rummage."
            ))
        })?;
    rt.block_on(async {
        let builder = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        let client = route
            .apply(builder)
            .and_then(|b| b.build())
            .map_err(|err| {
                log::error!("无法创建下载用的网络客户端：{err}");
                Error::Failed(tr!(
                    "无法创建网络连接，请检查代理设置后重试。",
                    "네트워크 연결을 만들 수 없습니다. 프록시 설정을 확인한 뒤 다시 시도하십시오.",
                    "Can't create a network connection. Check the proxy settings, then try again."
                ))
            })?;
        if !todo.is_empty() {
            fs::create_dir_all(dir).map_err(write_error)?;
        }
        // 每个仓库只读一次文件列表
        let mut lists: Vec<(&str, Option<Vec<Remote>>)> = Vec::new();
        for (i, f) in todo {
            let known = lists.iter().position(|(repo, _)| *repo == f.repo);
            let list = match known {
                Some(k) => &lists[k].1,
                None => {
                    rep.start(ModelTaskPhase::Preparing, f.file, i, 0);
                    let info = remote_info(&client, route, hub, &f, cancel).await?;
                    lists.push((f.repo, info));
                    &lists[lists.len() - 1].1
                }
            };
            if let Some(list) = list {
                check_remote(list, f.file)?;
            }
            let url = Url::parse(&hub.file_url(&f)).map_err(|_| {
                Error::Failed(tr!(
                    "下载源地址格式不正确，请检查后重试。",
                    "다운로드 소스 주소 형식이 올바르지 않습니다. 확인한 뒤 다시 시도하십시오.",
                    "The download source address is invalid. Check it and try again."
                ))
            })?;
            let part = dir.join(format!("{}.part", f.file.name));
            fetch(&client, route, &url, f.file, &part, i, cancel, &mut rep).await?;
            fs::rename(&part, dir.join(f.file.name)).map_err(rename_error)?;
        }
        if !qnn_todo.is_empty() {
            fs::create_dir_all(qnn_dir).map_err(write_error)?;
        }
        for (i, e) in qnn_todo {
            let part = qnn_dir.join(format!("{}.part", e.file.name));
            fetch_qnn(&client, route, e, &part, i, cancel, &mut rep).await?;
            fs::rename(&part, qnn_dir.join(e.file.name)).map_err(rename_error)?;
        }
        Ok(())
    })
}

/// 用 HTTP Range 取 `[start, start + len)`；须回 206 且起点相符
async fn range(
    client: &Client,
    url: &Url,
    proxied: bool,
    start: u64,
    len: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    let req = client
        .get(url.clone())
        .header(RANGE, format!("bytes={start}-{}", start + len - 1));
    let resp = tokio::select! {
        r = req.send() => r.map_err(|err| net_error(&err, proxied))?,
        () = cancelled(cancel) => return Err(Error::Cancelled),
    };
    let status = resp.status();
    if !status.is_success() {
        log::warn!("下载 NPU 运行库时 Maven Central 返回 {status}");
        return Err(status_error(status));
    }
    if status != StatusCode::PARTIAL_CONTENT || range_start(&resp) != Some(start) {
        log::warn!("下载 NPU 运行库时 Maven Central 没有按请求返回部分内容（{status}）。");
        return Err(Error::Failed(tr!(
            "下载 NPU 运行库时服务器返回的数据不对，请稍后重试。",
            "NPU 런타임을 다운로드하는 중 서버가 잘못된 데이터를 보냈습니다. 잠시 후 다시 시도하십시오.",
            "The server returned unexpected data while downloading the NPU runtime. Try again later."
        )));
    }
    let body = tokio::select! {
        b = resp.bytes() => b.map_err(|err| net_error(&err, proxied))?,
        () = cancelled(cancel) => return Err(Error::Cancelled),
    };
    if body.len() as u64 != len {
        return Err(Error::Failed(tr!(
            "下载 NPU 运行库时连接中断，请重试。",
            "NPU 런타임을 다운로드하는 중 연결이 끊겼습니다. 다시 시도하십시오.",
            "The connection dropped while downloading the NPU runtime. Try again."
        )));
    }
    Ok(body.to_vec())
}

/// 从 QNN 包中取出一个条目，解压到 `part` 并核对（见模块说明）
async fn fetch_qnn(
    client: &Client,
    route: &Route,
    entry: &'static QnnEntry,
    part: &Path,
    index: usize,
    cancel: &AtomicBool,
    rep: &mut Reporter<'_>,
) -> Result<(), Error> {
    let url = Url::parse(npu::QNN_URL).expect("内置的地址格式正确");
    let proxied = route.proxies(&url);
    rep.start(ModelTaskPhase::Downloading, &entry.file, index, 0);
    // 本地文件头 30 字节：签名、……、文件名长度（26）、扩展字段长度（28）
    let head = range(client, &url, proxied, entry.offset, 30, cancel).await?;
    let le16 = |i: usize| u64::from(u16::from_le_bytes([head[i], head[i + 1]]));
    if head[..4] != *b"PK\x03\x04" {
        log::warn!("QNN 包中 {} 的位置不是本地文件头。", entry.file.name);
        return Err(Error::Failed(tr!(
            "下载的 NPU 运行库与预期的不同，请稍后重试。",
            "다운로드한 NPU 런타임이 예상과 다릅니다. 잠시 후 다시 시도하십시오.",
            "The downloaded NPU runtime isn't what was expected. Try again later."
        )));
    }
    let data = entry.offset + 30 + le16(26) + le16(28);
    let packed = range(client, &url, proxied, data, entry.compressed, cancel).await?;
    rep.advance(entry.file.size / 2);
    let mut raw = Vec::with_capacity(entry.file.size as usize);
    flate2::read::DeflateDecoder::new(&packed[..])
        .take(entry.file.size + 1)
        .read_to_end(&mut raw)
        .map_err(|err| {
            log::warn!("无法解压 QNN 包中的 {}：{err}", entry.file.name);
            Error::Failed(tr!(
                "下载的 NPU 运行库已损坏，请重试。",
                "다운로드한 NPU 런타임이 손상되었습니다. 다시 시도하십시오.",
                "The downloaded NPU runtime is corrupted. Try again."
            ))
        })?;
    if raw.len() as u64 != entry.file.size || hex(&Sha256::digest(&raw)) != entry.file.sha256 {
        return Err(Error::Failed(tr!(
            "下载的 \"{}\" 不完整或已损坏，请重试。",
            "다운로드한 \"{}\" 파일이 불완전하거나 손상되었습니다. 다시 시도하십시오.",
            "The downloaded \"{}\" is incomplete or corrupted. Try again.",
            entry.file.name
        )));
    }
    let mut out = File::create(part).map_err(write_error)?;
    out.write_all(&raw).map_err(write_error)?;
    out.sync_all().map_err(write_error)?;
    rep.advance(entry.file.size);
    Ok(())
}

/// 文件列表接口中的一项：(路径, 大小, SHA-256)
type Remote = (String, u64, String);

/// 读取钉定提交的文件列表。`Ok(None)`：接口不可用（错误状态、内容无法解析），改用已知表；连不上下载源时返回错误
async fn remote_info(
    client: &Client,
    route: &Route,
    hub: &Hub,
    pinned: &Pinned,
    cancel: &AtomicBool,
) -> Result<Option<Vec<Remote>>, Error> {
    let Ok(url) = Url::parse(&hub.list_url(pinned)) else {
        return Err(Error::Failed(tr!(
            "下载源地址格式不正确，请检查后重试。",
            "다운로드 소스 주소 형식이 올바르지 않습니다. 확인한 뒤 다시 시도하십시오.",
            "The download source address is invalid. Check it and try again."
        )));
    };
    let send = async {
        let resp = client.get(url.clone()).send().await?;
        if !resp.status().is_success() {
            log::warn!(
                "下载源的文件列表接口返回 {}，改用内置的文件信息。",
                resp.status()
            );
            return Ok(None);
        }
        resp.bytes().await.map(Some)
    };
    let body = tokio::select! {
        r = send => r.map_err(|err| net_error(&err, route.proxies(&url)))?,
        () = cancelled(cancel) => return Err(Error::Cancelled),
    };
    let Some(body) = body else {
        return Ok(None);
    };
    let list = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| match hub {
            Hub::HuggingFace(_) => {
                let list = v.as_array()?.iter().filter_map(|e| {
                    // 非 LFS 的小文件没有 lfs 字段，跳过
                    let lfs = e.get("lfs")?;
                    Some((
                        e.get("path")?.as_str()?.to_string(),
                        lfs.get("size")?.as_u64()?,
                        lfs.get("oid")?.as_str()?.to_lowercase(),
                    ))
                });
                Some(list.collect::<Vec<_>>())
            }
            // 出错时以 `Code` 为准；目录项的 `Sha256` 为空，跳过
            Hub::ModelScope(_) => {
                if v.get("Code")?.as_u64()? != 200 {
                    return None;
                }
                let files = v.get("Data")?.get("Files")?.as_array()?;
                let list = files.iter().filter_map(|e| {
                    let sha = e.get("Sha256")?.as_str().filter(|s| !s.is_empty())?;
                    Some((
                        e.get("Path")?.as_str()?.to_string(),
                        e.get("Size")?.as_u64()?,
                        sha.to_lowercase(),
                    ))
                });
                Some(list.collect::<Vec<_>>())
            }
        });
    if list.is_none() {
        log::warn!("无法解析下载源的文件列表，改用内置的文件信息。");
    }
    Ok(list)
}

/// 下载源列出的文件信息须与已知表一致
fn check_remote(list: &[Remote], file: &ModelFile) -> Result<(), Error> {
    match list.iter().find(|(path, _, _)| path == file.name) {
        Some((_, size, sha)) if *size != file.size || sha != file.sha256 => {
            log::warn!(
                "下载源列出的 {} 为 {size} 字节、SHA-256 {sha}，与内置记录不一致。",
                file.name
            );
            Err(Error::Failed(tr!(
                "下载源提供的 \"{}\" 与 Rummage 需要的版本不一致，请换一个下载源。",
                "다운로드 소스의 \"{}\" 파일이 Rummage에 필요한 버전과 다릅니다. 다른 다운로드 소스를 사용하십시오.",
                "The \"{}\" offered by the download source isn't the version Rummage needs. Use a different download source.",
                file.name
            )))
        }
        Some(_) => Ok(()),
        None => {
            log::warn!(
                "下载源的文件列表中没有 {}，按内置的文件信息校验。",
                file.name
            );
            Ok(())
        }
    }
}

/// 206 回复中 Content-Range 的起点
fn range_start(resp: &reqwest::Response) -> Option<u64> {
    let v = resp.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    v.strip_prefix("bytes ")?.split('-').next()?.parse().ok()
}

/// 下载一个文件到 `part`，续传已有的部分；完成并校验通过后返回，`.part` 由调用方改名
#[allow(clippy::too_many_arguments)]
async fn fetch(
    client: &Client,
    route: &Route,
    url: &Url,
    file: &'static ModelFile,
    part: &Path,
    index: usize,
    cancel: &AtomicBool,
    rep: &mut Reporter<'_>,
) -> Result<(), Error> {
    let proxied = route.proxies(url);
    let mut have = fs::metadata(part).map_or(0, |m| m.len());
    if have > file.size {
        fs::remove_file(part).map_err(write_error)?;
        have = 0;
    }
    let mut hasher = Sha256::new();
    if have > 0 {
        rep.start(ModelTaskPhase::Checking, file, index, 0);
        (hasher, have) = hash_file(part, cancel, rep)?;
    }
    if have < file.size {
        // 续传的回复不对时删掉 `.part` 再从头请求一次；从头请求时不带 Range，最多再循环一次
        let mut resp = loop {
            let mut req = client.get(url.clone());
            if have > 0 {
                req = req.header(RANGE, format!("bytes={have}-"));
            }
            let resp = tokio::select! {
                r = req.send() => r.map_err(|err| net_error(&err, proxied))?,
                () = cancelled(cancel) => return Err(Error::Cancelled),
            };
            let status = resp.status();
            // 已下载的部分无效（文件在服务器上变了）：删掉，下次从头下载
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                fs::remove_file(part).map_err(write_error)?;
                return Err(Error::Failed(tr!(
                    "下载源不接受续传，已删除下载了一部分的文件，请重试。",
                    "다운로드 소스가 이어받기를 지원하지 않아 일부만 받은 파일을 삭제했습니다. 다시 시도하십시오.",
                    "The download source doesn't support resuming, so the partly downloaded file was deleted. Try again."
                )));
            }
            if !status.is_success() {
                log::warn!("下载 {} 时下载源返回 {status}", file.name);
                return Err(status_error(status));
            }
            if status == StatusCode::PARTIAL_CONTENT && range_start(&resp) != Some(have) {
                if have == 0 {
                    log::warn!("下载 {} 时下载源返回了没有请求过的部分内容。", file.name);
                    return Err(Error::Failed(tr!(
                        "下载源返回的数据不完整，请重试或换一个下载源。",
                        "다운로드 소스가 보낸 데이터가 불완전합니다. 다시 시도하거나 다른 다운로드 소스를 사용하십시오.",
                        "The download source returned incomplete data. Try again or use a different download source."
                    )));
                }
                log::warn!(
                    "下载源返回的续传位置与已下载的 {have} 字节不符，从头下载 {}。",
                    file.name
                );
                drop(resp);
                fs::remove_file(part).map_err(write_error)?;
                have = 0;
                hasher = Sha256::new();
                continue;
            }
            break resp;
        };
        let resume = have > 0 && resp.status() == StatusCode::PARTIAL_CONTENT;
        let mut out = if resume {
            OpenOptions::new().append(true).open(part)
        } else {
            have = 0;
            hasher = Sha256::new();
            File::create(part)
        }
        .map_err(write_error)?;
        rep.start(ModelTaskPhase::Downloading, file, index, have);
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c.map_err(|err| net_error(&err, proxied))?,
                () = cancelled(cancel) => return Err(Error::Cancelled),
            };
            let Some(chunk) = chunk else {
                break;
            };
            if have + chunk.len() as u64 > file.size {
                drop(out);
                fs::remove_file(part).map_err(write_error)?;
                return Err(Error::Failed(tr!(
                    "下载源返回的 \"{}\" 比预期的大，可能不是所需的版本，请换一个下载源。",
                    "다운로드 소스가 보낸 \"{}\" 파일이 예상보다 큽니다. 필요한 버전이 아닐 수 있으니 다른 다운로드 소스를 사용하십시오.",
                    "The \"{}\" from the download source is larger than expected and may be the wrong version. Use a different download source.",
                    file.name
                )));
            }
            out.write_all(&chunk).map_err(write_error)?;
            hasher.update(&chunk);
            have += chunk.len() as u64;
            rep.advance(have);
        }
        out.sync_all().map_err(write_error)?;
    }
    if have != file.size {
        return Err(Error::Failed(tr!(
            "下载中断（\"{}\" 只收到 {have} / {} 字节），请重试，会从中断处继续。",
            "다운로드가 중단되었습니다(\"{}\" {have} / {}바이트만 받음). 다시 시도하면 중단된 곳부터 이어받습니다.",
            "The download was interrupted (\"{}\": received only {have} / {} bytes). Try again to resume where it stopped.",
            file.name,
            file.size
        )));
    }
    if hex(&hasher.finalize()) != file.sha256 {
        fs::remove_file(part).map_err(write_error)?;
        return Err(Error::Failed(tr!(
            "下载的 \"{}\" 不完整或已损坏，已删除。请重试，或换一个下载源。",
            "다운로드한 \"{}\" 파일이 불완전하거나 손상되어 삭제했습니다. 다시 시도하거나 다른 다운로드 소스를 사용하십시오.",
            "The downloaded \"{}\" was incomplete or corrupted and has been deleted. Try again or use a different download source.",
            file.name
        )));
    }
    Ok(())
}

fn status_error(status: StatusCode) -> Error {
    let code = status.as_u16();
    Error::Failed(match code {
        404 => tr!(
            "下载源上找不到模型文件，请换一个下载源。",
            "다운로드 소스에서 모델 파일을 찾을 수 없습니다. 다른 다운로드 소스를 사용하십시오.",
            "The model file wasn't found on the download source. Use a different download source."
        ),
        401 | 403 => tr!(
            "下载源拒绝了访问（HTTP {code}），请换一个下载源。",
            "다운로드 소스가 접근을 거부했습니다(HTTP {code}). 다른 다운로드 소스를 사용하십시오.",
            "The download source denied access (HTTP {code}). Use a different download source."
        ),
        407 => tr!(
            "代理服务器要求身份验证，请检查代理地址中的用户名和密码。",
            "프록시 서버에서 인증을 요구합니다. 프록시 주소의 사용자 이름과 비밀번호를 확인하십시오.",
            "The proxy server requires authentication. Check the username and password in the proxy address."
        ),
        429 => tr!(
            "下载源限制了请求频率，请稍后再试或换一个下载源。",
            "다운로드 소스가 요청 횟수를 제한했습니다. 잠시 후 다시 시도하거나 다른 다운로드 소스를 사용하십시오.",
            "The download source is limiting requests. Try again later or use a different download source."
        ),
        500..=599 => tr!(
            "下载源暂时不可用（HTTP {code}），请稍后再试或换一个下载源。",
            "다운로드 소스를 일시적으로 사용할 수 없습니다(HTTP {code}). 잠시 후 다시 시도하거나 다른 다운로드 소스를 사용하십시오.",
            "The download source is temporarily unavailable (HTTP {code}). Try again later or use a different download source."
        ),
        _ => tr!(
            "下载源返回了错误（HTTP {code}），请换一个下载源。",
            "다운로드 소스가 오류를 반환했습니다(HTTP {code}). 다른 다운로드 소스를 사용하십시오.",
            "The download source returned an error (HTTP {code}). Use a different download source."
        ),
    })
}

/// 网络错误的中文原因。`proxied` 为这次请求经过自定义代理；
/// 跟随系统时不知道是否经过代理，只能从 reqwest 的隧道错误判断
fn net_error(err: &reqwest::Error, proxied: bool) -> Error {
    let mut chain = err.to_string();
    let mut kind = None;
    let mut code = None;
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        chain.push_str(": ");
        chain.push_str(&e.to_string());
        if let Some(io) = e.downcast_ref::<io::Error>() {
            kind = kind.or(Some(io.kind()));
            code = code.or(io.raw_os_error());
        }
        source = e.source();
    }
    log::warn!("下载模型时网络出错：{chain}");
    let lower = chain.to_lowercase();
    let tls = ["certificate", "tls", "handshake", "corrupt message"]
        .iter()
        .any(|s| lower.contains(s));
    let retry = trs!(
        "请检查网络，或换一个下载源、设置代理",
        "네트워크를 확인하거나, 다른 다운로드 소스를 사용하거나, 프록시를 설정하십시오",
        "Check your network, use a different download source, or set up a proxy"
    );
    let reason = if lower.contains("proxy authorization required") {
        tr!(
            "代理服务器要求身份验证，请检查代理地址中的用户名和密码",
            "프록시 서버에서 인증을 요구합니다. 프록시 주소의 사용자 이름과 비밀번호를 확인하십시오",
            "The proxy server requires authentication. Check the username and password in the proxy address"
        )
    } else if lower.contains("tunnel error: unsuccessful") {
        tr!(
            "代理服务器未能连接到下载源，请检查代理软件的设置，或换一个下载源",
            "프록시 서버가 다운로드 소스에 연결하지 못했습니다. 프록시 프로그램 설정을 확인하거나 다른 다운로드 소스를 사용하십시오",
            "The proxy server couldn't reach the download source. Check your proxy software settings or use a different download source"
        )
    } else if lower.contains("tunnel error: failed to create underlying connection")
        || (proxied && err.is_connect() && !tls)
    {
        tr!(
            "无法连接代理服务器，请检查代理地址和端口是否正确、代理软件是否已启动",
            "프록시 서버에 연결할 수 없습니다. 프록시 주소와 포트가 올바른지, 프록시 프로그램이 실행 중인지 확인하십시오",
            "Can't connect to the proxy server. Check that the proxy address and port are correct and the proxy software is running"
        )
    } else if err.is_timeout() || kind == Some(ErrorKind::TimedOut) {
        tr!(
            "连接下载源超时，{retry}",
            "다운로드 소스 연결 시간이 초과되었습니다. {retry}",
            "Connecting to the download source timed out. {retry}"
        )
    } else if lower.contains("dns error") {
        tr!(
            "无法解析下载源的域名，{retry}",
            "다운로드 소스의 도메인을 찾을 수 없습니다. {retry}",
            "Can't resolve the download source's domain. {retry}"
        )
    } else if kind == Some(ErrorKind::ConnectionRefused) {
        tr!(
            "连接被拒绝，请检查下载源地址是否正确",
            "연결이 거부되었습니다. 다운로드 소스 주소가 올바른지 확인하십시오",
            "The connection was refused. Check that the download source address is correct"
        )
    } else if matches!(
        kind,
        Some(ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted)
    ) {
        tr!(
            "连接被重置，{retry}",
            "연결이 재설정되었습니다. {retry}",
            "The connection was reset. {retry}"
        )
    } else if lower.contains("connection closed before message completed") {
        tr!(
            "下载源未返回数据就关闭了连接，请重试",
            "다운로드 소스가 데이터를 보내지 않고 연결을 닫았습니다. 다시 시도하십시오",
            "The download source closed the connection without sending data. Try again"
        )
    } else if tls {
        tr!(
            "TLS 握手失败，无法验证下载源的证书，请检查网络环境或代理设置",
            "TLS 핸드셰이크에 실패해 다운로드 소스의 인증서를 확인할 수 없습니다. 네트워크 환경이나 프록시 설정을 확인하십시오",
            "The TLS handshake failed, so the download source's certificate couldn't be verified. Check your network or proxy settings"
        )
    } else if err.is_redirect() {
        tr!(
            "下载源重定向次数过多，请换一个下载源",
            "다운로드 소스의 리디렉션이 너무 많습니다. 다른 다운로드 소스를 사용하십시오",
            "The download source redirected too many times. Use a different download source"
        )
    } else {
        tr!(
            "网络连接出错，{retry}",
            "네트워크 연결 오류가 발생했습니다. {retry}",
            "A network error occurred. {retry}"
        )
    };
    let note = code.map_or_else(String::new, |c| {
        tr!("（错误码 {c}）", "(오류 코드 {c})", " (error code {c})")
    });
    Error::Failed(tr!(
        "{reason}{note}。",
        "{reason}{note}.",
        "{reason}{note}."
    ))
}

/// 导入用户选择的模型文件：按大小在 `known` 中找到对应的文件，复制到 `dir` 并校验 SHA-256。
/// 返回导入（或已存在且正确）的文件；有不认识的文件时一个也不导入
pub fn import(
    paths: &[PathBuf],
    known: &[&'static ModelFile],
    dir: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Progress),
) -> Result<Vec<&'static ModelFile>, Error> {
    let mut picked = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy());
        rummage_index::ensure_local(path).map_err(|err| Error::Failed(err.to_string()))?;
        let len = fs::metadata(path)
            .map_err(|err| read_error(path, err))?
            .len();
        let Some(file) = known.iter().find(|f| f.size == len) else {
            let names: Vec<&str> = known.iter().map(|f| f.name).collect();
            return Err(Error::Failed(tr!(
                "\"{name}\" 不是 Rummage 使用的模型文件，请选择 {}。",
                "\"{name}\" 파일은 Rummage 모델 파일이 아닙니다. {} 파일을 선택하십시오.",
                "\"{name}\" isn't a model file Rummage uses. Choose {}.",
                names
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(trs!(" 或 ", " 또는 ", " or "))
            )));
        };
        picked.push((path, *file));
    }
    let mut rep = Reporter::new(progress, picked.len());
    fs::create_dir_all(dir).map_err(write_error)?;
    let mut done = Vec::new();
    for (i, (src, file)) in picked.into_iter().enumerate() {
        let target = dir.join(file.name);
        rep.start(ModelTaskPhase::Checking, file, i, 0);
        if !valid(&target, file, cancel, &mut rep)? {
            rep.start(ModelTaskPhase::Copying, file, i, 0);
            let part = dir.join(format!("{}.import.part", file.name));
            let sha = copy_hashed(src, &part, cancel, &mut rep)?;
            if sha != file.sha256 {
                fs::remove_file(&part).map_err(write_error)?;
                let name = src.file_name().unwrap_or_default().to_string_lossy();
                return Err(Error::Failed(tr!(
                    "\"{name}\" 的内容与 Rummage 需要的模型不一致，可能已损坏或不是所需的版本。",
                    "\"{name}\" 파일의 내용이 Rummage에 필요한 모델과 다릅니다. 손상되었거나 필요한 버전이 아닐 수 있습니다.",
                    "The contents of \"{name}\" don't match the model Rummage needs. It may be corrupted or the wrong version."
                )));
            }
            fs::rename(&part, &target).map_err(rename_error)?;
        }
        if !done.iter().any(|f: &&ModelFile| f.name == file.name) {
            done.push(file);
        }
    }
    Ok(done)
}

/// 完整校验 `path` 处已安装的模型文件；文件不存在或大小不对时为 false
pub fn verify(
    path: &Path,
    file: &'static ModelFile,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Progress),
) -> Result<bool, Error> {
    let mut rep = Reporter::new(progress, 1);
    rep.start(ModelTaskPhase::Checking, file, 0, 0);
    valid(path, file, cancel, &mut rep)
}

/// 删除 `dir` 中的模型文件及其 `.part`、`.import.part`；不存在的跳过。`label` 是给用户看的模型名称。
/// 返回模型文件本身是否存在（否则只删了下载或导入了一部分的文件）
pub fn remove(dir: &Path, file: &ModelFile, label: &str) -> Result<bool, Error> {
    let installed = dir.join(file.name).is_file();
    let names = [
        file.name.to_string(),
        format!("{}.part", file.name),
        format!("{}.import.part", file.name),
    ];
    for name in names {
        match fs::remove_file(dir.join(&name)) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            // ERROR_ACCESS_DENIED、ERROR_SHARING_VIOLATION：文件仍被打开（推理进程没有及时退出，或被杀毒软件等扫描）
            Err(err) if cfg!(windows) && matches!(err.raw_os_error(), Some(5 | 32)) => {
                log::warn!("无法删除 {name}，文件正被使用：{err}");
                return Err(Error::Failed(tr!(
                    "{label}正被使用，暂时无法删除，请稍后再试。",
                    "{label} 파일이 사용 중이어서 지금은 삭제할 수 없습니다. 잠시 후 다시 시도하십시오.",
                    "The {label} is in use and can't be deleted right now. Try again later."
                )));
            }
            Err(err) => {
                log::warn!("无法删除 {name}：{err}");
                return Err(Error::Failed(tr!(
                    "无法删除{label}，请检查数据目录的权限后重试。",
                    "{label} 파일을 삭제할 수 없습니다. 데이터 폴더 권한을 확인한 뒤 다시 시도하십시오.",
                    "Can't delete the {label}. Check the data folder permissions, then try again."
                )));
            }
        }
    }
    Ok(installed)
}

/// 删除 LiteRT 在 `dir` 中为模型文件 `name` 生成的权重与程序缓存（`<name>.…`、`<name>_…`）。缓存名带模型文件的修改时间
/// 与大小，重新下载的模型不会再用旧缓存；删不掉的只记录警告
pub fn remove_caches(dir: &Path, name: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let is_cache = file
            .to_string_lossy()
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(['.', '_']));
        if is_cache && let Err(err) = fs::remove_file(entry.path()) {
            log::warn!("无法删除模型缓存 {}：{err}", entry.path().display());
        }
    }
}

/// 复制文件并计算 SHA-256；取消或出错时删除写了一半的目标
fn copy_hashed(
    src: &Path,
    dst: &Path,
    cancel: &AtomicBool,
    rep: &mut Reporter,
) -> Result<String, Error> {
    let mut input = File::open(src).map_err(|err| read_error(src, err))?;
    let mut out = File::create(dst).map_err(write_error)?;
    let res = (|| {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BUF];
        let mut n = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let len = input.read(&mut buf).map_err(|err| read_error(src, err))?;
            if len == 0 {
                break;
            }
            out.write_all(&buf[..len]).map_err(write_error)?;
            hasher.update(&buf[..len]);
            n += len as u64;
            rep.advance(n);
        }
        out.sync_all().map_err(write_error)?;
        Ok(hex(&hasher.finalize()))
    })();
    if res.is_err() {
        drop(out);
        let _ = fs::remove_file(dst);
    }
    res
}

#[cfg(test)]
mod tests;
