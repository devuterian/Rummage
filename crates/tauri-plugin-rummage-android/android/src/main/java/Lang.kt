package com.marierie.rummage.storage

import android.content.Context

/**
 * 通知等 Kotlin 侧文字的界面语言：前端确定语言后经 `setLanguage` 存进 SharedPreferences，
 * 进程由 [PowerJob] 新启动时也能读到。未设置时为韩语
 */
object Lang {
  private const val PREFS = "rummage_lang"
  private const val KEY = "lang"
  private val SUPPORTED = setOf("zh", "ko", "en")

  fun set(ctx: Context, lang: String) {
    if (lang !in SUPPORTED) return
    ctx.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putString(KEY, lang).apply()
  }

  fun get(ctx: Context): String =
    ctx.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(KEY, null)?.takeIf { it in SUPPORTED } ?: "ko"

  fun tr(ctx: Context, zh: String, ko: String, en: String): String = when (get(ctx)) {
    "zh" -> zh
    "en" -> en
    else -> ko
  }
}
