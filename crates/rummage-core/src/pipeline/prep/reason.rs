//! 失败原因：预处理失败时记入作业表、显示在“失败文件”列表中的文字。
//! 各错误类型的 `Display` 带路径，适合写日志，不适合放进列表（系统错误码、ffmpeg 与 Xberg 的英文输出等细节在产生错误处写日志）：
//! 列表中另有文件名与路径，原因只需一句中文说明出了什么问题、能怎么办，主语统一为“此文件”；涉及设置项时写出参数表中的名称。
//! 原始错误由 `prep::failed` 写日志。
use std::io;

use rummage_embed::Error as EmbedError;
use rummage_extract::chunk::ChunkError;
use rummage_extract::{DocError, ReadError};
use rummage_index::{DbError, ENCRYPTED, FsError, HashError, IoCause, io_kind};
use rummage_media::ffmpeg::MediaError;
use rummage_proto::i18n::Lang;
use rummage_proto::{params, tr, trs};

pub(in crate::pipeline) trait Reason: std::fmt::Display {
    /// 给用户看的中文整句，不含路径
    fn reason(&self) -> String;
}

/// 读写索引数据库出错时的原因，写库线程记失败时也用它
pub(in crate::pipeline) fn db_reason() -> &'static str {
    trs!(
        "读写索引时出错，请稍后再试。",
        "색인을 읽고 쓰는 중 오류가 발생했습니다. 잠시 후 다시 시도하십시오.",
        "An error occurred while reading or writing the index. Try again later."
    )
}

/// 参数表中设置项的名称。参数表只有中文，这里用到的几项按界面语言给出
pub(crate) fn label(key: &str) -> &str {
    match (rummage_proto::i18n::lang(), key) {
        (Lang::Ko, "max_doc_mb") => "문서 크기 제한(MB)",
        (Lang::Ko, "max_image_mp") => "이미지 화소 제한(메가픽셀)",
        (Lang::Ko, "max_audio_min") => "오디오·동영상 길이 제한(분)",
        (Lang::En, "max_doc_mb") => "Document size limit (MB)",
        (Lang::En, "max_image_mp") => "Image pixel limit (megapixels)",
        (Lang::En, "max_audio_min") => "Audio and video duration limit (min)",
        _ => params::def(key).map_or(key, |d| d.label),
    }
}

fn io_reason(err: &io::Error) -> String {
    match io_kind(err) {
        IoCause::Busy => trs!(
            "此文件正被其他程序占用，无法读取。",
            "다른 프로그램이 이 파일을 사용 중이어서 읽을 수 없습니다.",
            "This file is in use by another program and can't be read."
        ),
        IoCause::NotFound => trs!(
            "找不到此文件，它可能已被移动或删除。",
            "이 파일을 찾을 수 없습니다. 이동되었거나 삭제되었을 수 있습니다.",
            "This file can't be found. It may have been moved or deleted."
        ),
        IoCause::Denied => trs!(
            "没有读取此文件的权限。",
            "이 파일을 읽을 권한이 없습니다.",
            "No permission to read this file."
        ),
        IoCause::Other => trs!(
            "无法读取此文件。",
            "이 파일을 읽을 수 없습니다.",
            "This file can't be read."
        ),
    }
    .into()
}

impl Reason for io::Error {
    fn reason(&self) -> String {
        io_reason(self)
    }
}

impl Reason for FsError {
    fn reason(&self) -> String {
        match self {
            FsError::Placeholder(_) => tr!(
                "此文件仅在云端，尚未下载到本机，不能读取内容。",
                "온라인 전용 파일이라 이 기기에 내려받기 전에는 내용을 읽을 수 없습니다.",
                "This is an online-only file that hasn't been downloaded to this device, so its contents can't be read."
            ),
            FsError::Unsupported { .. } => tr!(
                "不支持读取此文件所在的位置。",
                "이 파일이 있는 위치는 읽기를 지원하지 않습니다.",
                "Reading from this file's location isn't supported."
            ),
            FsError::Inaccessible(_) => tr!(
                "无法访问此文件，请确认它存在且可以打开。",
                "이 파일에 접근할 수 없습니다. 파일이 있고 열 수 있는지 확인하십시오.",
                "This file can't be accessed. Make sure it exists and can be opened."
            ),
        }
    }
}

impl Reason for HashError {
    fn reason(&self) -> String {
        match self {
            HashError::Fs(err) => err.reason(),
            HashError::Io { source, .. } => io_reason(source),
        }
    }
}

impl Reason for ReadError {
    fn reason(&self) -> String {
        match self {
            ReadError::Fs(err) => err.reason(),
            ReadError::Io { source, .. } => io_reason(source),
            ReadError::TooLarge { .. } => tr!(
                "此文件超过设置中的“{}”，不读取正文。",
                "설정의 ‘{}’을 넘는 파일이라 본문을 읽지 않습니다.",
                "This file exceeds ‘{}’ in Settings, so its text isn't read.",
                label("max_doc_mb")
            ),
            ReadError::NotText(_) => tr!(
                "此文件不是文本文件，不读取正文。",
                "텍스트 파일이 아니라 본문을 읽지 않습니다.",
                "This isn't a text file, so its text isn't read."
            ),
        }
    }
}

impl Reason for DocError {
    fn reason(&self) -> String {
        match self {
            DocError::Read(err) => err.reason(),
            DocError::Encrypted(_) => ENCRYPTED.into(),
            DocError::Extract { .. } => tr!(
                "无法提取此文件的正文，文件可能已损坏或格式不受支持。",
                "이 파일의 본문을 추출할 수 없습니다. 파일이 손상되었거나 지원하지 않는 형식일 수 있습니다.",
                "Can't extract text from this file. It may be corrupted or in an unsupported format."
            ),
        }
    }
}

impl Reason for rummage_media::Error {
    fn reason(&self) -> String {
        match self {
            rummage_media::Error::Io { source, .. } => io_reason(source),
            rummage_media::Error::Decode { .. } => tr!(
                "无法解码此文件，文件可能已损坏或格式不受支持。",
                "이 파일을 디코딩할 수 없습니다. 파일이 손상되었거나 지원하지 않는 형식일 수 있습니다.",
                "Can't decode this file. It may be corrupted or in an unsupported format."
            ),
            rummage_media::Error::TooLarge { .. } => tr!(
                "此文件的像素数超过设置中的“{}”，不识别内容。",
                "픽셀 수가 설정의 ‘{}’을 넘는 파일이라 내용을 인식하지 않습니다.",
                "This file's pixel count exceeds ‘{}’ in Settings, so its content isn't recognized.",
                label("max_image_mp")
            ),
            // 图片与视频画面都会遇到，统一说“画面”
            rummage_media::Error::UnsupportedSize { width, height } => tr!(
                "此文件的画面尺寸为 {width}×{height}，过于细长或为空，无法识别内容。",
                "이 파일의 화면 크기가 {width}×{height}로 너무 길쭉하거나 비어 있어 내용을 인식할 수 없습니다.",
                "This file's frame size is {width}×{height}, which is too narrow or empty to recognize."
            ),
            rummage_media::Error::InvalidPixels { .. } => tr!(
                "此文件的画面数据异常，无法识别内容。",
                "이 파일의 화면 데이터가 비정상이라 내용을 인식할 수 없습니다.",
                "This file's image data is invalid, so its content can't be recognized."
            ),
        }
    }
}

impl Reason for MediaError {
    fn reason(&self) -> String {
        match self {
            MediaError::MissingTool(_) => tr!(
                "缺少读取音频和视频所需的组件，请重新安装 Rummage。",
                "오디오와 동영상을 읽는 데 필요한 구성 요소가 없습니다. Rummage를 다시 설치하십시오.",
                "A component needed to read audio and video is missing. Reinstall Rummage."
            ),
            MediaError::Spawn { .. } => tr!(
                "无法启动读取音频和视频所需的组件，请重新启动 Rummage 后重试。",
                "오디오와 동영상을 읽는 데 필요한 구성 요소를 시작할 수 없습니다. Rummage를 다시 시작한 뒤 다시 시도하십시오.",
                "Can't start the component needed to read audio and video. Restart Rummage, then try again."
            ),
            MediaError::Timeout { what, secs } => {
                let what = media_what(what);
                tr!(
                    "读取此文件中的{what}超过 {secs} 秒仍未完成，已放弃。",
                    "이 파일의 {what}을(를) {secs}초 넘게 읽지 못해 중단했습니다.",
                    "Reading the {what} in this file took longer than {secs} seconds, so it was skipped."
                )
            }
            MediaError::Decode { what, .. } => {
                let what = media_what(what);
                tr!(
                    "无法读取此文件中的{what}，文件可能已损坏或格式不受支持。",
                    "이 파일의 {what}을(를) 읽을 수 없습니다. 파일이 손상되었거나 지원하지 않는 형식일 수 있습니다.",
                    "Can't read the {what} in this file. It may be corrupted or in an unsupported format."
                )
            }
            MediaError::NoAudio => tr!(
                "此文件中没有音轨。",
                "이 파일에는 오디오 트랙이 없습니다.",
                "This file has no audio track."
            ),
            MediaError::NoVideo => tr!(
                "此文件中没有视频画面。",
                "이 파일에는 동영상 화면이 없습니다.",
                "This file has no video picture."
            ),
            MediaError::TooLong => tr!(
                "此文件超过设置中的“{}”，不识别内容。",
                "설정의 ‘{}’을 넘는 파일이라 내용을 인식하지 않습니다.",
                "This file exceeds ‘{}’ in Settings, so its content isn't recognized.",
                label("max_audio_min")
            ),
            MediaError::Unsupported => tr!(
                "当前系统不支持识别音频和视频的内容。",
                "이 시스템에서는 오디오와 동영상 내용을 인식할 수 없습니다.",
                "This system doesn't support recognizing audio and video content."
            ),
        }
    }
}

/// `MediaError` 中的“音频”“视频”（由 rummage-media 给出）按界面语言换成对应的词
fn media_what(what: &str) -> &str {
    match (what, rummage_proto::i18n::lang()) {
        ("音频", Lang::Ko) => "오디오",
        ("音频", Lang::En) => "audio",
        ("视频", Lang::Ko) => "동영상",
        ("视频", Lang::En) => "video",
        _ => what,
    }
}

impl Reason for DbError {
    fn reason(&self) -> String {
        db_reason().into()
    }
}

impl Reason for ChunkError<EmbedError> {
    fn reason(&self) -> String {
        match self {
            // 主语为此文件，同 `embed`；`user_message` 以显卡为主语，留给设备提示用
            ChunkError::Count(EmbedError::OutOfMemory(_)) => tr!(
                "识别此文件时显存或内存不足。",
                "이 파일을 인식하는 중 GPU 메모리나 시스템 메모리가 부족했습니다.",
                "Ran out of GPU or system memory while recognizing this file."
            ),
            ChunkError::Count(err) => err.user_message(),
            ChunkError::PrefixTooLong => tr!(
                "此文件的文件名过长，无法识别内容。",
                "파일 이름이 너무 길어 내용을 인식할 수 없습니다.",
                "This file's name is too long to recognize its content."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn reasons_have_no_path_or_detail() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let hash = HashError::Io {
            path: r"D:\资料\a.txt".into(),
            source: denied,
        };
        assert_eq!(hash.reason(), "没有读取此文件的权限。");
        let decode = MediaError::Decode { what: "视频" };
        assert_eq!(
            decode.reason(),
            "无法读取此文件中的视频，文件可能已损坏或格式不受支持。"
        );
        // 设置项写出参数表中的名称
        let big = rummage_media::Error::TooLarge {
            path: PathBuf::from("a.png"),
        };
        assert_eq!(
            big.reason(),
            "此文件的像素数超过设置中的“图片像素上限（百万像素）”，不识别内容。"
        );
        assert_eq!(
            MediaError::TooLong.reason(),
            "此文件超过设置中的“音视频时长上限（分钟）”，不识别内容。"
        );
        let img = rummage_media::Error::Decode {
            path: PathBuf::from(r"D:\a.png"),
        };
        assert!(!img.reason().contains("a.png"));
        let doc = DocError::Extract(r"D:\a.pdf".into());
        assert_eq!(
            doc.reason(),
            "无法提取此文件的正文，文件可能已损坏或格式不受支持。"
        );
        assert_eq!(DbError::Busy.reason(), db_reason());
        let oom = ChunkError::Count(EmbedError::OutOfMemory("分词".into()));
        assert_eq!(oom.reason(), "识别此文件时显存或内存不足。");
        #[cfg(windows)]
        assert_eq!(
            io::Error::from_raw_os_error(32).reason(),
            "此文件正被其他程序占用，无法读取。"
        );
    }
}
