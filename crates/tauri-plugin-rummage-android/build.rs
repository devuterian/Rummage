fn main() {
    // 只向前端开放 set_language（界面语言交给 Kotlin 侧的通知文字），其余命令只由 Rust 侧调用；登记 Android 工程
    tauri_plugin::Builder::new(&["set_language"])
        .android_path("android")
        .build();
}
