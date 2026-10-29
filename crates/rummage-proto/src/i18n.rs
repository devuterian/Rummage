//! 界面语言：中文、韩语、英语。进程内只有一份当前语言（原子变量），给用户看的文字用 `tr!` 按它选择。
//! 未设置时为中文（测试按中文断言）。外观设置中的语言由界面首次启动时按系统语言选好并保存；
//! 核心启动时、读写外观设置时，界面进程在取得、保存外观设置时调用 `set_lang`。日志不翻译。
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Zh,
    Ko,
    En,
}

#[cfg(not(test))]
static LANG: AtomicU8 = AtomicU8::new(0);
// 测试并行运行：每个线程各有一份，切换语言的测试不影响其他按中文断言的测试
#[cfg(test)]
thread_local! {
    static LANG: AtomicU8 = const { AtomicU8::new(0) };
}

fn with_lang<T>(f: impl FnOnce(&AtomicU8) -> T) -> T {
    #[cfg(not(test))]
    return f(&LANG);
    #[cfg(test)]
    return LANG.with(f);
}

pub fn lang() -> Lang {
    match with_lang(|l| l.load(Ordering::Relaxed)) {
        1 => Lang::Ko,
        2 => Lang::En,
        _ => Lang::Zh,
    }
}

pub fn set_lang(lang: Lang) {
    let v = match lang {
        Lang::Zh => 0,
        Lang::Ko => 1,
        Lang::En => 2,
    };
    with_lang(|l| l.store(v, Ordering::Relaxed));
}

/// 按当前语言选用三种写法之一并 `format!`：`tr!("中文 {n}", "한국어 {n}", "English {n}")`，
/// 也可以带位置参数：`tr!("共 {} 个", "{}개", "{} items", n)`。结果是 `String`
#[macro_export]
macro_rules! tr {
    ($zh:literal, $ko:literal, $en:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Zh => format!($zh),
            $crate::i18n::Lang::Ko => format!($ko),
            $crate::i18n::Lang::En => format!($en),
        }
    };
    ($zh:literal, $ko:literal, $en:literal, $($arg:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Zh => format!($zh, $($arg)+),
            $crate::i18n::Lang::Ko => format!($ko, $($arg)+),
            $crate::i18n::Lang::En => format!($en, $($arg)+),
        }
    };
}

/// 同 `tr!`，但只选不格式化，结果是 `&'static str`：用于静态表与不带参数的常量文字
#[macro_export]
macro_rules! trs {
    ($zh:literal, $ko:literal, $en:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Zh => $zh,
            $crate::i18n::Lang::Ko => $ko,
            $crate::i18n::Lang::En => $en,
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_lang() {
        let n = 3;
        set_lang(Lang::Ko);
        assert_eq!(tr!("共 {n} 个", "{n}개", "{n} items"), "3개");
        assert_eq!(tr!("共 {} 个", "{}개", "{} items", n), "3개");
        set_lang(Lang::En);
        assert_eq!(trs!("搜索", "검색", "Search"), "Search");
        set_lang(Lang::Zh);
        assert_eq!(trs!("搜索", "검색", "Search"), "搜索");
    }
}
