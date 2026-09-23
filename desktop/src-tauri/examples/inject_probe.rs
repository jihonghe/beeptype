//! 注入探针：验证"不碰剪贴板、直接把文字打进去"在真实应用里到底靠不靠得住。
//!
//! 用法（在 `desktop/src-tauri` 目录下）：
//!
//! ```text
//! cargo run --example inject_probe -- "要注入的文字，可以带中文和 English 123"
//! ```
//!
//! 它会先等 5 秒（给你把光标放到目标输入框的时间），然后逐字注入，最后报告耗时。
//! 想试换行就把 `\n` 写进参数（例如 `"第一行\n第二行"`）——**重点看它会不会触发"发送"**。
//!
//! 要观察的四件事：
//! 1. **有没有丢字/错字**（这是注入路线最大的风险：它没有剪贴板那样的"读回校验"）
//! 2. **耗时**（逐字注入，长文会明显慢）
//! 3. **换行会不会被目标应用当成回车**（聊天软件里会直接发出去）
//! 4. **哪些应用不吃注入**（游戏、提权窗口、带反注入的输入框等）
use enigo::{Enigo, Keyboard, Settings};
use std::time::{Duration, Instant};

fn main() {
    let text = std::env::args().nth(1).unwrap_or_else(|| {
        "这是一段中文注入测试：包含标点、English words 与 123。".to_string()
    });
    let gap = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);
    println!(
        "{} 秒内把光标放到目标输入框（要注入 {} 个字）……",
        gap,
        text.chars().count()
    );
    std::thread::sleep(Duration::from_secs(gap));

    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            println!("enigo 初始化失败（当前会话支持按键注入吗？）：{e}");
            return;
        }
    };

    let start = Instant::now();
    match enigo.text(&text) {
        Ok(()) => {
            let d = start.elapsed();
            let n = text.chars().count();
            println!("注入完成：{n} 字，耗时 {d:?}（约 {:.1} 字/毫秒）", n as f64 / d.as_millis().max(1) as f64);
        }
        Err(e) => println!("注入失败：{e}"),
    }
}
