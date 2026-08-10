// Tauri 应用入口（发布版隐藏 Windows 控制台窗口）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 注册 panic 钩子：把 panic 信息与调用栈写入临时目录日志文件
    // （Windows GUI 子系统看不到 stdout/stderr，便于排查启动与运行期问题）
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let text = format!("[Motrix] PANIC: {info}\n{backtrace}\n");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("motrix-panic.log"))
        {
            use std::io::Write;
            let _ = writeln!(f, "{text}");
        }
        prev_hook(info);
    }));

    // 调用库入口（库拆分便于集成测试与后续扩展单实例等能力）
    motrix_tauri_lib::run()
}
