//! 启动时确定数据目录：
//! 指向的目录无法使用（例如数据目录在移动硬盘上而硬盘没插）时，在创建窗口前弹出对话框，
//! 让用户重试、改用默认目录（再确认一次）或退出。无法显示对话框时本次使用默认目录，指向文件不动
use std::path::Path;

use rummage_datadir::{Error, Fallback, Startup};
use rummage_proto::{tr, trs};

/// 指向的目录无法使用时的选择
#[derive(Debug, Clone, Copy, PartialEq)]
enum Choice {
    Retry,
    /// 删除指向文件，改用默认目录
    UseDefault,
    /// 无法弹出对话框时：本次使用默认目录，指向文件保持不动
    Skip,
    Exit,
}

/// 指向的目录无法使用：重试、改用默认目录（再确认一次）或退出
fn ask(err: &Error, default: &Path) -> Choice {
    let (heading, content, footer) = match err {
        Error::Missing(dir) => (
            trs!(
                "找不到数据目录",
                "데이터 폴더 없음",
                "Data folder not found"
            ),
            tr!(
                "存储设备可能未连接。",
                "저장 장치가 연결되지 않았을 수 있습니다.",
                "The storage device may not be connected."
            ),
            tr!(
                "数据目录：{}",
                "데이터 폴더: {}",
                "Data folder: {}",
                dir.display()
            ),
        ),
        Error::Invalid {
            reason,
            label,
            path,
        } => (
            trs!(
                "数据目录设置无效",
                "데이터 폴더 설정이 잘못됨",
                "Invalid data folder setting"
            ),
            reason.clone(),
            format!("{label}：{}", path.display()),
        ),
        _ => return Choice::Skip,
    };
    loop {
        match rummage_datadir::choose(
            heading,
            &content,
            &footer,
            &[
                trs!("重试", "다시 시도", "Retry"),
                trs!("改用默认目录", "기본 폴더 사용", "Use default folder"),
                trs!("退出", "종료", "Quit"),
            ],
        ) {
            // 无法显示对话框：本次使用默认目录，指向文件不动
            None if !rummage_datadir::can_choose() => return Choice::Skip,
            Some(0) => return Choice::Retry,
            Some(1) => {
                let old = match err {
                    Error::Missing(dir) => Some(dir),
                    // label 按界面语言生成（`rummage_datadir`），与那里的文字一致
                    Error::Invalid { label, path, .. }
                        if *label == trs!("数据目录", "데이터 폴더", "Data folder") =>
                    {
                        Some(path)
                    }
                    _ => None,
                };
                let footer = match old {
                    Some(old) => tr!(
                        "原数据目录：{}\n默认目录：{}",
                        "기존 데이터 폴더: {}\n기본 폴더: {}",
                        "Previous data folder: {}\nDefault folder: {}",
                        old.display(),
                        default.display()
                    ),
                    None => tr!(
                        "默认目录：{}",
                        "기본 폴더: {}",
                        "Default folder: {}",
                        default.display()
                    ),
                };
                let confirm = rummage_datadir::choose(
                    trs!(
                        "改用默认目录？",
                        "기본 폴더를 사용하시겠습니까?",
                        "Use the default folder?"
                    ),
                    trs!(
                        "原目录中的索引和模型不会转移，需要重新设置文件夹并下载模型。",
                        "기존 폴더의 색인과 모델은 옮겨지지 않습니다. 폴더를 다시 설정하고 모델을 다시 다운로드해야 합니다.",
                        "The index and models in the previous folder won't be moved. You'll need to set up folders and download the models again."
                    ),
                    &footer,
                    &[
                        trs!("改用默认目录", "기본 폴더 사용", "Use default folder"),
                        trs!("返回", "돌아가기", "Back"),
                    ],
                );
                if confirm == Some(0) {
                    return Choice::UseDefault;
                }
            }
            _ => return Choice::Exit,
        }
    }
}

/// 确定数据目录，有待迁移的更改时先完成迁移；指向的目录无法使用时询问用户，选择退出时直接退出进程
pub fn prepare() -> Startup {
    let mut fallback = None;
    loop {
        let startup = rummage_datadir::prepare(fallback);
        let err = match &startup.dir {
            Err(err @ (Error::Missing(_) | Error::Invalid { .. })) => err,
            _ => return startup,
        };
        let default = rummage_datadir::default_location().unwrap_or_default();
        match ask(err, &default) {
            Choice::Retry => {}
            Choice::UseDefault => fallback = Some(Fallback::UseDefault),
            Choice::Skip => fallback = Some(Fallback::Skip),
            Choice::Exit => std::process::exit(0),
        }
    }
}
