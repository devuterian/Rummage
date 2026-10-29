//! 系统托盘：关闭主窗口后界面进程留在托盘，核心照常运行（设置为关闭时退出的除外，见 `lib`）。
//! 单击图标显示主窗口（已关闭时重新创建）；右键菜单提供显示窗口、暂停或恢复索引和退出。
//! 暂停状态一律以核心的 `IndexStatus` 为准，与界面看到的一致：连上核心、收到 `IndexProgress` 和断开时由 `core` 更新托盘，
//! 暂停或恢复后再取一次状态（`refresh`）。设置菜单和提示要等主线程执行，放在专用线程上，只处理最新的一次状态；
//! 菜单只在暂停状态变化时重建，悬停提示随进度更新。`refresh` 取到的状态可能比之后到达的进度事件旧，
//! 所以只在暂停状态变化（或此前未知）时采用
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rummage_proto::{IndexState, IndexStatus, PauseReason, Reply, Request, tr, trs};
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use crate::{core, pause, platform};

const TRAY_ID: &str = "main";

/// 决定菜单内容的状态
#[derive(Debug, Clone, PartialEq)]
enum View {
    /// 核心未连接或索引尚未启动，暂停项不可用
    Unknown,
    Running,
    /// 因全屏、电池等原因自动暂停，仍可再手动暂停
    Auto(&'static str),
    /// 手动暂停，附到期时间的说明
    Manual(String),
}

/// 待更新线程处理的最新状态，以及它是否来自 `refresh`
static NEXT: Mutex<Option<(Option<IndexStatus>, bool)>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
/// 串行化主窗口的创建，同时到达的两次打开请求不会建出两个窗口
static CREATING: Mutex<()> = Mutex::new(());

const LOCK: &str = "持锁期间只读写字段，不会 panic";

pub fn create<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Rummage")
        .menu(&menu(app, &View::Unknown)?)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    let app = app.clone();
    std::thread::Builder::new()
        .name("tray-update".into())
        .spawn(move || {
            // 上次设置的菜单状态与悬停提示，没有变化时不重复设置
            let mut last: Option<(View, String)> = None;
            // 界面语言变化（`settings::set_appearance`、连上核心时）后按新语言重建菜单
            let mut last_lang = rummage_proto::i18n::lang();
            loop {
                let (status, from_refresh) = {
                    let mut next = NEXT.lock().expect(LOCK);
                    loop {
                        if let Some(item) = next.take() {
                            break item;
                        }
                        next = WAKE.wait(next).expect(LOCK);
                    }
                };
                let lang = rummage_proto::i18n::lang();
                if lang != last_lang {
                    last_lang = lang;
                    last = None;
                }
                let view = view(status.as_ref());
                let prev_view = last.as_ref().map(|p| &p.0);
                if from_refresh && prev_view.is_some_and(|v| *v != View::Unknown && *v == view) {
                    continue;
                }
                apply(&app, &mut last, view, tooltip(status.as_ref()));
            }
        })?;
    Ok(())
}

/// 主窗口关闭、准备退出时隐藏图标
pub fn hide<R: Runtime>(app: &AppHandle<R>) {
    if let Some(tray) = app.tray_by_id(TRAY_ID)
        && let Err(err) = tray.set_visible(false)
    {
        log::warn!("无法隐藏托盘图标：{err}");
    }
}

/// 安装程序没能启动、界面留下时恢复图标
pub fn show<R: Runtime>(app: &AppHandle<R>) {
    if let Some(tray) = app.tray_by_id(TRAY_ID)
        && let Err(err) = tray.set_visible(true)
    {
        log::warn!("无法显示托盘图标：{err}");
    }
}

fn menu<R: Runtime>(app: &AppHandle<R>, view: &View) -> tauri::Result<Menu<R>> {
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(
                app,
                "show",
                trs!("显示 Rummage", "Rummage 열기", "Show Rummage"),
                true,
                None::<&str>,
            )?,
            &PredefinedMenuItem::separator(app)?,
        ],
    )?;
    let line = match view {
        View::Auto(text) => Some(text.to_string()),
        View::Manual(text) => Some(text.clone()),
        _ => None,
    };
    if let Some(text) = line {
        menu.append(&MenuItem::new(app, text, false, None::<&str>)?)?;
    }
    if matches!(view, View::Manual(_)) {
        menu.append(&MenuItem::with_id(
            app,
            "resume",
            trs!("恢复索引", "색인 계속", "Resume indexing"),
            true,
            None::<&str>,
        )?)?;
    } else {
        menu.append(&Submenu::with_items(
            app,
            trs!("暂停索引", "색인 일시정지", "Pause indexing"),
            *view != View::Unknown,
            &[
                &MenuItem::with_id(
                    app,
                    "pause_hour",
                    trs!("暂停 1 小时", "1시간 일시정지", "Pause for 1 hour"),
                    true,
                    None::<&str>,
                )?,
                &MenuItem::with_id(
                    app,
                    "pause_tomorrow",
                    trs!("暂停到明天", "내일까지 일시정지", "Pause until tomorrow"),
                    true,
                    None::<&str>,
                )?,
                &MenuItem::with_id(
                    app,
                    "pause_manual",
                    trs!(
                        "直到手动恢复",
                        "직접 계속할 때까지",
                        "Until resumed manually"
                    ),
                    true,
                    None::<&str>,
                )?,
            ],
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        trs!("退出", "종료", "Quit"),
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

fn on_menu<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let until = match event.id().as_ref() {
        "show" => return show_main(app),
        "quit" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { core::quit(&app).await });
            return;
        }
        "resume" => None,
        "pause_hour" => Some(pause::PauseFor::Hour),
        "pause_tomorrow" => Some(pause::PauseFor::Tomorrow),
        "pause_manual" => Some(pause::PauseFor::Manual),
        _ => return,
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let res = match until {
            Some(until) => pause::pause(&app, until).await,
            None => pause::resume(&app).await,
        };
        if let Err(err) = res {
            log::warn!("托盘暂停或恢复索引失败：{}", err.message);
        }
    });
}

/// 显示并聚焦主窗口；窗口已关闭时重新创建
pub fn show_main<R: Runtime>(app: &AppHandle<R>) {
    // 在事件处理中同步创建 WebView2 窗口会死锁（见 `WebviewWindowBuilder::build` 的说明），放到阻塞线程里
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _creating = CREATING.lock().expect(LOCK);
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            return;
        }
        if let Err(err) = crate::create_main_window(&app) {
            log::error!("无法打开主窗口：{err}");
        }
    });
}

/// 向核心取一次索引状态并更新托盘
pub async fn refresh<R: Runtime>(app: &AppHandle<R>) {
    let status = match core::request(app, Request::IndexStatus).await {
        Ok(Reply::Index(status)) => Some(status),
        _ => None,
    };
    send(status, true);
}

/// 按核心的索引状态更新菜单与悬停提示；`None` 表示核心未连接或索引尚未启动
pub fn update(status: Option<&IndexStatus>) {
    send(status.cloned(), false);
}

fn send(status: Option<IndexStatus>, from_refresh: bool) {
    *NEXT.lock().expect(LOCK) = Some((status, from_refresh));
    WAKE.notify_one();
}

fn view(status: Option<&IndexStatus>) -> View {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    match status.map(|s| s.paused) {
        None => View::Unknown,
        Some(None) => View::Running,
        // 已到期、核心还没清除（每 5 秒判定一次）
        Some(Some(PauseReason::Manual { until: Some(ms) })) if ms <= now => View::Manual(tr!(
            "即将恢复索引",
            "곧 색인 계속",
            "Resuming indexing soon"
        )),
        Some(Some(PauseReason::Manual { until })) => {
            View::Manual(match until.map(platform::local_time_at).transpose() {
                Ok(t) => manual_text(t, platform::local_time()),
                Err(err) => {
                    log::warn!("无法换算暂停到期时间：{err}");
                    tr!("已手动暂停", "직접 일시정지함", "Paused manually")
                }
            })
        }
        Some(Some(reason)) => View::Auto(auto_text(reason)),
    }
}

fn apply<R: Runtime>(
    app: &AppHandle<R>,
    last: &mut Option<(View, String)>,
    view: View,
    tip: String,
) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if last.as_ref().map(|p| &p.0) != Some(&view) {
        match menu(app, &view) {
            Ok(m) => {
                if let Err(err) = tray.set_menu(Some(m)) {
                    log::warn!("无法更新托盘菜单：{err}");
                }
            }
            Err(err) => log::warn!("无法创建托盘菜单：{err}"),
        }
    }
    if last.as_ref().map(|p| &p.1) != Some(&tip)
        && let Err(err) = tray.set_tooltip(Some(&tip))
    {
        log::warn!("无法更新托盘提示：{err}");
    }
    *last = Some((view, tip));
}

/// 文字与界面的索引提示（`src/components/IndexHint.tsx`）一致
fn auto_text(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::Manual { .. } => trs!("已手动暂停", "직접 일시정지함", "Paused manually"),
        PauseReason::Sleeping => trs!(
            "系统即将睡眠或刚唤醒，稍后继续",
            "시스템 절전 직전이거나 막 깨어남, 잠시 후 계속",
            "System is about to sleep or just woke up, resuming shortly"
        ),
        PauseReason::DiskLow { .. } => trs!(
            "磁盘空间不足，已暂停",
            "디스크 공간 부족으로 일시정지됨",
            "Paused: low disk space"
        ),
        PauseReason::Quota => trs!(
            "已达到系统每天 6 小时的后台运行上限",
            "시스템의 하루 6시간 백그라운드 실행 한도에 도달함",
            "Reached the system's 6-hour daily background limit"
        ),
        PauseReason::Hot => trs!(
            "手机温度过高，已暂停",
            "휴대폰 온도가 높아 일시정지됨",
            "Paused: phone is too hot"
        ),
        PauseReason::Battery => trs!(
            "未接电源，已暂停",
            "전원이 연결되지 않아 일시정지됨",
            "Paused: not plugged in"
        ),
        PauseReason::LowBattery => trs!(
            "电量低，已暂停",
            "배터리 부족으로 일시정지됨",
            "Paused: low battery"
        ),
        PauseReason::ScreenOn => trs!(
            "屏幕亮着，已暂停",
            "화면이 켜져 있어 일시정지됨",
            "Paused: screen is on"
        ),
        PauseReason::Fullscreen => trs!(
            "有全屏程序在运行，已暂停",
            "전체 화면 프로그램 실행 중이라 일시정지됨",
            "Paused: a full-screen app is running"
        ),
    }
}

/// `until`、`today` 为本地时间 `[年, 月, 日, 时, 分, 秒]`；到期在今天时只写时刻
fn manual_text(until: Option<[u16; 6]>, today: [u16; 6]) -> String {
    let Some(t) = until else {
        return tr!(
            "已暂停，直到手动恢复",
            "일시정지됨, 직접 계속할 때까지",
            "Paused until resumed manually"
        );
    };
    let clock = format!("{}:{:02}", t[3], t[4]);
    if t[..3] == today[..3] {
        return tr!(
            "已暂停到 {clock}",
            "{clock}까지 일시정지됨",
            "Paused until {clock}"
        );
    }
    let (month, day) = (t[1], t[2]);
    tr!(
        "已暂停到 {month}月{day}日 {clock}",
        "{month}월 {day}일 {clock}까지 일시정지됨",
        "Paused until {month}/{day} {clock}"
    )
}

/// 与界面的索引提示一样，有待处理的文件时显示本轮的进度（`IndexStatus::round`）
fn tooltip(status: Option<&IndexStatus>) -> String {
    let Some((s, r)) = status
        .filter(|s| matches!(s.state, IndexState::Idle | IndexState::Running))
        .and_then(|s| s.round.map(|r| (s, r)))
    else {
        return "Rummage".into();
    };
    let (done, total) = (thousands(r.done), thousands(r.total));
    if s.paused.is_some() {
        tr!(
            "Rummage：索引已暂停 {done} / {total}",
            "Rummage: 색인 일시정지됨 {done} / {total}",
            "Rummage: indexing paused {done} / {total}"
        )
    } else {
        tr!(
            "Rummage：正在建立索引 {done} / {total}",
            "Rummage: 색인 생성 중 {done} / {total}",
            "Rummage: indexing {done} / {total}"
        )
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use rummage_proto::RoundStatus;

    use super::*;

    fn status(state: IndexState, paused: Option<PauseReason>) -> IndexStatus {
        IndexStatus {
            state,
            paused,
            slow: false,
            total: 5000,
            done: 1200,
            failed: 34,
            pending: 3766,
            by_kind: Default::default(),
            backend: None,
            error: None,
            eta_s: None,
            rebuild: None,
            round: Some(RoundStatus {
                done: 234,
                total: 4000,
            }),
        }
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn tooltip_shows_progress_only_while_indexing() {
        assert_eq!(tooltip(None), "Rummage");
        let s = status(IndexState::Running, None);
        assert_eq!(tooltip(Some(&s)), "Rummage：正在建立索引 234 / 4,000");
        let s = status(IndexState::Running, Some(PauseReason::Battery));
        assert_eq!(tooltip(Some(&s)), "Rummage：索引已暂停 234 / 4,000");
        assert_eq!(tooltip(Some(&status(IndexState::NoModel, None))), "Rummage");
        let done = IndexStatus {
            pending: 0,
            round: None,
            ..status(IndexState::Idle, None)
        };
        assert_eq!(tooltip(Some(&done)), "Rummage");
    }

    #[test]
    fn manual_pause_text() {
        let today = [2026, 10, 13, 15, 0, 0];
        assert_eq!(manual_text(None, today), "已暂停，直到手动恢复");
        assert_eq!(
            manual_text(Some([2026, 10, 13, 16, 5, 0]), today),
            "已暂停到 16:05"
        );
        assert_eq!(
            manual_text(Some([2026, 10, 14, 0, 0, 0]), today),
            "已暂停到 10月14日 0:00"
        );
    }
}
