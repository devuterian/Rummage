//! 向量计算的统一入口：其他 crate 只通过这里使用推理。
//!
//! - 输入：`Input` 区分查询、文档、图像、音频与视频窗口；任务前缀在本 crate 内拼接（`prefix`），调用方不拼。
//! - 输出：`Embedding` 带向量空间 ID（`space`）；向量都经过数值检查和 L2 归一化（`vector`）。
//! - 后端：`LocalBackend`（`local`）实现 `EmbeddingBackend`：一个调度线程按优先级（查询先于索引，同级先来先到）
//!   把请求逐个交给推理子进程 `rummage-core worker`。推理子进程用 llama.cpp，或用 LiteRT-LM（`litert`，
//!   `WorkerSpec::litert`；手机端的向量空间，电脑上只用于评测）。手机 NPU 的模型与运行库清单见 `npu`。
//! - 推理子进程（`worker`）：只在其中加载 llama.cpp；Windows 上放进 Job Object 并设内存上限；
//!   Android 上没有子进程，是应用进程内的线程（`android.rs`），帧协议相同。
//!   初始化与单次推理都有时限。默认只加载主模型，遇到图像或音频时才带上 mmproj 重启；空闲超时后结束。
//!   CPU 模式超时、崩溃后下次请求重新拉起，连续失败 3 次后不再重试；
//!   GPU 模式出错不再拉起（显存或内存分配失败只让这一项失败，下次请求重新拉起）；出现设备丢失时返回 `Error::DeviceLost`，由上层屏蔽该设备。
//!   GPU 由推理进程按描述与 PCI 号在它自己枚举出的设备中选定，原始序号也在推理进程中现算（`worker`）。
//! - 自检（`selftest`）：用黄金样本检查每个候选后端并选出最快的，结果按设备与版本缓存，未通过的 GPU 屏蔽。
//! - 协议见 `frame`；平台专有代码只在平台模块中（`windows.rs` / `android.rs` / `other.rs`）。

mod frame;
mod litert;
mod local;
pub mod npu;
pub mod prefix;
pub mod selftest;
pub mod space;
pub mod vector;
mod worker;

#[cfg(target_os = "android")]
#[path = "android.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "android")))]
#[path = "other.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use rummage_proto::tr;

pub use frame::{DeviceInfo, GpuInfo};
pub use litert::{Accel, TIERS as LITERT_TIERS};
pub use local::{Gpu, Limits, LocalBackend, WorkerSpec, list_devices};
pub use platform::adapters;
#[cfg(windows)]
pub use platform::event_xml;
#[cfg(target_os = "android")]
pub use platform::{Launcher, serve_embed_process, set_launcher};
pub use space::{Backend, SpaceConfig, SpaceId};
pub use worker::{LiteRtArgs, WorkerArgs, run_worker, run_worker_stdio};

/// 一块显卡适配器的属性（`adapters`），供诊断包使用；只查询属性，不加载推理运行库。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Adapter {
    pub description: String,
    /// PCI 厂商与设备 ID
    pub vendor_id: u32,
    pub device_id: u32,
    /// 专用显存
    pub vram_mb: u64,
    /// 用户模式驱动版本；查询失败时为空
    pub driver: Option<String>,
}

/// CPU 设备显示的名称：处理器型号，读不到时为“CPU”。只用于显示，设备 ID 与自检缓存的键都不含它
pub fn cpu_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| platform::cpu_name().unwrap_or_else(|| "CPU".to_string()))
}

/// 推理进程连续失败多少次后不再重新拉起（由上层回退到下一个后端）。
pub const MAX_FAILURES: u32 = 3;
/// 结束推理进程后最多等它退出多久；驱动挂起时进程可能结束不了，到期就不再等它，按设备失效处理
#[cfg(not(target_os = "android"))]
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(10);
/// Android 上推理是进程内的线程，中止只在计算节点之间生效：1120 档图像在 i7-12700H 上最慢的单个节点约 2.4 秒，手机更慢
#[cfg(target_os = "android")]
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("无法读取模型文件 \"{}\"：{source}", .path.display())]
    ModelIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("模型文件 \"{}\" 的大小为 {actual} 字节，应为 {expected} 字节，文件可能不完整或不是所需的版本。", .path.display())]
    ModelSize {
        path: PathBuf,
        actual: u64,
        expected: u64,
    },
    #[error("未加载多模态模型文件")]
    NoMmproj,
    #[error("输入无效：{0}")]
    InvalidInput(String),
    /// 文字查询无法编码（超过上下文长度等）；文件内容的同类错误为 `InvalidInput`，两者给用户看的整句不同
    #[error("查询无法编码：{0}")]
    QueryTooLong(String),
    /// 视频窗口超过 token 上限；与 `InvalidInput` 一样与后端是否正常无关
    #[error("输入过长：{0}")]
    TooLong(String),
    #[error("向量数值异常：{0}")]
    BadVector(String),
    #[error("无法启动推理进程：{0}")]
    Spawn(String),
    #[error("推理引擎启动失败：{0}")]
    InitFailed(String),
    /// 推理进程可见的 GPU 与要求的不符；`LocalBackend` 内部换一个推理进程重试，不会返回给调用方。
    #[error("推理引擎启动失败：{0}")]
    GpuMismatch(String),
    /// 推理进程找不到选中的显卡（外接显卡已拔出、被停用等）；之后的请求都返回它，由上层换下一个能用的设备，不屏蔽。
    #[error("找不到显卡，它可能已拔出、被停用或驱动未加载：{0}")]
    GpuNotFound(String),
    #[error("推理引擎在 {} 秒内没有就绪，已结束推理进程。", .0.as_secs_f64())]
    InitTimeout(Duration),
    #[error("单次推理超过 {} 秒，已结束推理进程。", .0.as_secs_f64())]
    InferTimeout(Duration),
    #[error("推理进程异常退出：{0}")]
    Crashed(String),
    /// 供上层屏蔽该设备；之后的请求都返回它。
    #[error("显卡设备丢失，本次运行不再使用它。\n{0}")]
    DeviceLost(String),
    /// 推理出错（输入无效的情况为 `InvalidInput`）；CPU 模式计入连续失败。
    #[error("推理失败：{0}")]
    Inference(String),
    #[error("推理进程的输出不符合协议：{0}")]
    Protocol(String),
    /// GPU 后端失效，或 CPU 模式连续失败 `MAX_FAILURES` 次；之后的请求都返回它，由上层回退。
    #[error("推理引擎不可用：{0}")]
    Unavailable(String),
    /// GPU 模式连续 `MAX_FAILURES` 次显存或内存分配失败（如显存一直被别的程序占满）；之后的请求都返回它。
    /// 与驱动无关，上层换下一块显卡
    #[error("显存或内存不足：{0}")]
    OutOfMemory(String),
    /// 推理进程在 `KILL_GRACE` 内结束不了（驱动挂死等）；之后的请求都返回它，`restart` 也不清除。
    /// 显卡上的由上层屏蔽这块卡、停用所有显卡改用 CPU；CPU 上的本次运行不再识别内容
    #[error("推理进程结束不了：{0}")]
    Stuck(String),
    #[error("推理调度线程已停止。")]
    Stopped,
    #[error("黄金样本无效：{0}")]
    Golden(String),
}

/// 单个输入用尽显存或内存时 `Error::Inference` 的内容，原样显示给用户
pub(crate) const OUT_OF_MEMORY: &str = "显存或内存不足，无法处理此文件。可以在「设置 → 索引」中降低“图片识别精度”，或在「设置 → 高级」中调高“内容识别进程内存上限（GB）”。";

impl Error {
    /// 显示给用户的整句（文件的索引状态、设备检测结果等，不只针对文件，所以只有缺模型、输入无法识别这类确实针对文件的才写“此文件”）。`Display` 带函数名、返回码等细节，只写进日志
    pub fn user_message(&self) -> String {
        match self {
            Error::ModelIo { .. } | Error::ModelSize { .. } => tr!(
                "模型文件无法读取或已损坏，请重新下载模型。",
                "모델 파일을 읽을 수 없거나 손상되었습니다. 모델을 다시 다운로드하십시오.",
                "The model file can't be read or is corrupted. Download the model again."
            ),
            Error::NoMmproj => tr!(
                "尚未下载图片与音视频模型，无法识别此文件的内容。",
                "이미지·오디오·동영상 모델을 아직 다운로드하지 않아 이 파일의 내용을 인식할 수 없습니다.",
                "The image, audio and video model hasn't been downloaded, so this file's content can't be recognized."
            ),
            Error::InvalidInput(_) => tr!(
                "此文件的内容无法识别。",
                "이 파일의 내용을 인식할 수 없습니다.",
                "This file's content can't be recognized."
            ),
            Error::QueryTooLong(_) => tr!(
                "搜索内容过长，请缩短后再试。",
                "검색어가 너무 깁니다. 줄인 뒤 다시 시도하십시오.",
                "The search is too long. Shorten it and try again."
            ),
            Error::TooLong(_) => tr!(
                "内容过长，无法识别。",
                "내용이 너무 길어 인식할 수 없습니다.",
                "The content is too long to recognize."
            ),
            Error::BadVector(_) => tr!(
                "识别结果异常，无法使用。",
                "인식 결과가 비정상이라 사용할 수 없습니다.",
                "The recognition result is invalid and can't be used."
            ),
            Error::Spawn(_) | Error::InitFailed(_) | Error::GpuMismatch(_) => tr!(
                "内容识别无法启动。",
                "내용 인식을 시작할 수 없습니다.",
                "Content recognition can't start."
            ),
            Error::GpuNotFound(_) => tr!(
                "找不到所选的显卡，它可能已拔出、被停用或驱动未加载。",
                "선택한 그래픽 카드(GPU)를 찾을 수 없습니다. 분리되었거나 사용 중지되었거나 드라이버가 로드되지 않았을 수 있습니다.",
                "The selected GPU can't be found. It may have been unplugged or disabled, or its driver isn't loaded."
            ),
            Error::InitTimeout(d) => tr!(
                "内容识别在 {} 秒内没有启动完成，已停止。",
                "내용 인식이 {}초 안에 시작되지 않아 중지했습니다.",
                "Content recognition didn't start within {} seconds and was stopped.",
                d.as_secs_f64()
            ),
            Error::InferTimeout(d) => tr!(
                "识别内容超过 {} 秒，已停止。",
                "내용 인식이 {}초를 넘어 중지했습니다.",
                "Recognizing the content took longer than {} seconds and was stopped.",
                d.as_secs_f64()
            ),
            Error::Crashed(_) => tr!(
                "内容识别意外中止。",
                "내용 인식이 예기치 않게 중단되었습니다.",
                "Content recognition stopped unexpectedly."
            ),
            Error::DeviceLost(_) => tr!(
                "显卡出现错误，本次运行不再使用它。",
                "그래픽 카드(GPU)에 오류가 생겨 이번 실행에서는 더 이상 사용하지 않습니다.",
                "The GPU ran into an error and won't be used for the rest of this session."
            ),
            Error::Inference(msg) if msg == OUT_OF_MEMORY => tr!(
                "显存或内存不足，无法处理此文件。可以在「设置 → 索引」中降低“图片识别精度”，或在「设置 → 高级」中调高“内容识别进程内存上限（GB）”。",
                "GPU 메모리나 메모리가 부족해 이 파일을 처리할 수 없습니다. ‘설정 → 색인’에서 ‘이미지 인식 정밀도’를 낮추거나, ‘설정 → 고급’에서 ‘내용 인식 프로세스 메모리 상한(GB)’을 높이십시오.",
                "Not enough GPU memory or RAM to process this file. Lower “Image recognition precision” in Settings → Index, or raise “Content recognition process memory limit (GB)” in Settings → Advanced."
            ),
            Error::Inference(_) | Error::Protocol(_) => tr!(
                "识别内容时出错。",
                "내용을 인식하는 중 오류가 발생했습니다.",
                "An error occurred while recognizing the content."
            ),
            Error::Unavailable(_) => tr!(
                "内容识别不可用。",
                "내용 인식을 사용할 수 없습니다.",
                "Content recognition is unavailable."
            ),
            Error::OutOfMemory(_) => tr!(
                "显存或内存不足，无法继续使用这块显卡。",
                "GPU 메모리나 메모리가 부족해 이 그래픽 카드(GPU)를 더 이상 사용할 수 없습니다.",
                "Not enough GPU memory or RAM to keep using this GPU."
            ),
            Error::Stuck(_) => tr!(
                "内容识别进程停止响应且无法结束，建议重新启动电脑。",
                "내용 인식 프로세스가 응답하지 않고 종료되지도 않습니다. 컴퓨터를 다시 시작하는 것이 좋습니다.",
                "The content recognition process stopped responding and can't be ended. Restarting the computer is recommended."
            ),
            Error::Stopped => tr!(
                "内容识别已停止。",
                "내용 인식이 중지되었습니다.",
                "Content recognition has stopped."
            ),
            Error::Golden(_) => tr!(
                "设备检测所需的样本文件无效，请重新安装 Rummage。",
                "기기 검사에 필요한 샘플 파일이 올바르지 않습니다. Rummage를 다시 설치하십시오.",
                "The sample files needed for the device check are invalid. Reinstall Rummage."
            ),
        }
    }
}

/// 一个待编码的输入。文本由本 crate 加任务前缀（前缀方案 A）；图像不加前缀。
#[derive(Debug, Clone)]
pub enum Input {
    Query(String),
    Document {
        title: Option<String>,
        text: String,
    },
    /// 已由 `rummage_media::prepare` / `fit` 按视觉 token 档位 `tokens` 缩放的图像，向量属于该档位的图像空间。
    Image {
        rgb: rummage_media::Rgb,
        tokens: u32,
    },
    /// 以图搜图的查询：图像同 `Image`，可附一段文字描述，图文合成一条输入、得到一个向量（主模型卡的交错输入）。
    /// 文字加查询前缀放在图像之前；文字为空白时与 `Image` 完全相同。向量属于该档位的图像空间
    ImageQuery {
        rgb: rummage_media::Rgb,
        tokens: u32,
        text: String,
    },
    /// 16 kHz 单声道 f32 采样（`rummage_media::audio`），不超过 30 秒；不加前缀。
    /// 向量属于预处理参数版本为 `preprocess`（`space::audio_preprocess`）的音频空间
    Audio {
        samples: Vec<f32>,
        preprocess: String,
    },
    /// 一个视频窗口（`rummage_media::video::assemble`），不加前缀；经 `mtmd_tokenize` 后超过 `max_tokens` 时
    /// 返回 `Error::TooLong`。向量属于预处理参数版本为 `preprocess`（`space::video_preprocess`）的视频空间
    Video {
        parts: Vec<rummage_media::video::VideoPart>,
        preprocess: String,
        max_tokens: usize,
    },
}

/// 交互查询永远插队。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Query,
    Index,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    /// 已 L2 归一化。
    pub vector: Vec<f32>,
    pub space: SpaceId,
}

pub trait EmbeddingBackend: Send + Sync {
    /// 阻塞到结果返回；不要在异步执行器上直接调用。
    fn embed(&self, input: Input, priority: Priority) -> Result<Embedding, Error>;
    /// 文本输入加上任务前缀后的 token 数，含 `<bos>`、`<eos>`（切片上限含前缀与特殊 token）；
    /// 只分词不推理。图像与音频输入返回 `InvalidInput`。阻塞到结果返回。
    fn count_tokens(&self, input: &Input, priority: Priority) -> Result<usize, Error>;
    /// 修改空闲卸载的时限（参数 `engine_idle_min`），立即生效；没有推理进程的后端不需要实现。
    fn set_idle_timeout(&self, _idle: Duration) {}
    /// 结束推理进程并清除失败记录，下一次请求时重新拉起（唤醒后验证未通过时用）；阻塞到完成。
    /// 没有推理进程的后端不需要实现。
    fn restart(&self) -> Result<(), Error> {
        Ok(())
    }
}
