//! Android 界面用到的系统功能（Kotlin 部分见 crates/tauri-plugin-rummage-android 的 `StoragePlugin`）：
//! - `open_file`：用其他应用打开文件，同 Windows 只按索引中的文件 ID 打开，路径由后台服务查出；
//!   经 FileProvider 换成 content URI，只给这一次打开授予读取权限。命中位置（`at`）没有通用的传法，忽略。
//! - `reveal_file`：在文件管理器中打开文件所在的文件夹，同样按文件 ID 查出路径；换成外部存储提供者的文件夹 URI 交给能查看文件夹的应用。
//! - `share_file`：用系统分享面板分享文件，同样按文件 ID 查出路径，经 FileProvider 只给接收方授予读取权限。
//! - `move_task_to_back`：返回键在界面上已经没有可关闭的内容时把应用退到后台，不退出进程，后台建立索引继续。
//! - `safe_area`：全面屏的安全区，前端启动时取一次（之后的变化由 Kotlin 侧直接写进页面）
use rummage_proto::{ApiError, FileInfo, trs};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_rummage_android::{Android, Insets};

use super::storage::failed;
use crate::search::{error, file_info};

/// 文件所在的文件夹找不到或设备未连接时报错
fn reachable(info: &FileInfo) -> Result<(), ApiError> {
    if !info.offline {
        return Ok(());
    }
    Err(error(
        ApiError::BAD_REQUEST,
        if info.missing {
            trs!(
                "找不到文件所在的文件夹，它可能已被删除、改名或移走。",
                "파일이 있던 폴더를 찾을 수 없습니다. 삭제되었거나 이름이 바뀌었거나 옮겨졌을 수 있습니다.",
                "Couldn't find the file's folder. It may have been deleted, renamed or moved."
            )
        } else {
            trs!(
                "文件所在的设备未连接，请连接后重试。",
                "파일이 있는 기기가 연결되지 않았습니다. 연결한 뒤 다시 시도하십시오.",
                "The device containing this file is not connected. Connect it and try again."
            )
        },
    ))
}

#[tauri::command]
pub async fn open_file<R: Runtime>(app: AppHandle<R>, file_id: i64) -> Result<(), ApiError> {
    let info = file_info(&app, file_id).await?;
    reachable(&info)?;
    let opened = app
        .state::<Android<R>>()
        .open_file(info.path.as_ref())
        .await
        .map_err(|err| {
            failed(
                trs!(
                    "无法打开这个文件，它可能已被移动或删除。",
                    "이 파일을 열 수 없습니다. 옮겨졌거나 삭제되었을 수 있습니다.",
                    "Couldn't open this file. It may have been moved or deleted."
                ),
                err,
            )
        })?;
    if opened {
        return Ok(());
    }
    Err(error(
        ApiError::BAD_REQUEST,
        trs!(
            "没有可以打开此文件的应用。",
            "이 파일을 열 수 있는 앱이 없습니다.",
            "No app can open this file."
        ),
    ))
}

#[tauri::command]
pub async fn reveal_file<R: Runtime>(app: AppHandle<R>, file_id: i64) -> Result<(), ApiError> {
    let info = file_info(&app, file_id).await?;
    reachable(&info)?;
    let opened = app
        .state::<Android<R>>()
        .reveal_file(info.path.as_ref())
        .await
        .map_err(|err| {
            failed(
                trs!(
                    "无法打开文件所在的文件夹。",
                    "파일이 있는 폴더를 열 수 없습니다.",
                    "Couldn't open the file's folder."
                ),
                err,
            )
        })?;
    if opened {
        return Ok(());
    }
    Err(error(
        ApiError::BAD_REQUEST,
        trs!(
            "没有可以打开文件夹的文件管理器。",
            "폴더를 열 수 있는 파일 관리자가 없습니다.",
            "No file manager can open the folder."
        ),
    ))
}

#[tauri::command]
pub async fn share_file<R: Runtime>(app: AppHandle<R>, file_id: i64) -> Result<(), ApiError> {
    let info = file_info(&app, file_id).await?;
    reachable(&info)?;
    app.state::<Android<R>>()
        .share_file(info.path.as_ref())
        .await
        .map_err(|err| {
            failed(
                trs!(
                    "无法分享这个文件，它可能已被移动或删除。",
                    "이 파일을 공유할 수 없습니다. 옮겨졌거나 삭제되었을 수 있습니다.",
                    "Couldn't share this file. It may have been moved or deleted."
                ),
                err,
            )
        })
}

#[tauri::command]
pub async fn move_task_to_back<R: Runtime>(app: AppHandle<R>) -> Result<(), ApiError> {
    app.state::<Android<R>>()
        .move_to_back()
        .await
        .map_err(|err| {
            failed(
                trs!(
                    "无法把应用退到后台。",
                    "앱을 백그라운드로 보낼 수 없습니다.",
                    "Couldn't move the app to the background."
                ),
                err,
            )
        })
}

#[tauri::command]
pub async fn safe_area<R: Runtime>(app: AppHandle<R>) -> Result<Insets, ApiError> {
    app.state::<Android<R>>().safe_area().await.map_err(|err| {
        failed(
            trs!(
                "无法读取安全区。",
                "안전 영역을 읽을 수 없습니다.",
                "Couldn't read the safe area."
            ),
            err,
        )
    })
}
