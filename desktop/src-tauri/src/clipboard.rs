//! 剪贴板写入 + 读回校验。
//!
//! 写入是自动粘贴链条上唯一"我们可能判断错"的环节：有的机器上装着剪贴板
//! 管理器（Klipper / GPaste / CopyQ），可能在写入后立刻接管所有权，也可能
//! 写入被静默丢弃。这类机器差异不该按型号特调，也不该假定成功，而是当场验证：
//!
//!   · 读回的就是我们要写的 → 通过；
//!   · 读到别的内容（被其他进程持有）→ 重写一次再验；
//!   · 读不回来（无法判定）→ 只记日志，按原样继续（fail-open：不因为
//!     "校验不了"把本来能用的粘贴挡掉）。
//!
//! 校验的边界要说清楚：它只能保证"我们写入的那个剪贴板后端里是正确的文字"，
//! **不能**保证目标应用看到的内容——Wayland 会话下 arboard 可能退回 XWayland
//! 后端，而目标应用读的是原生 Wayland 剪贴板，那种不一致这里看不出来。那部分
//! 由"不支持自动粘贴的环境回退手动 Ctrl+V"兜底。

use log::{info, warn};
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

/// 写入尝试次数：首次 + 一次重写
const WRITE_ATTEMPTS: u8 = 2;

/// 写入文本到剪贴板并读回校验。
///
/// 返回 `true` 表示"剪贴板里可以认为就是这段文字"——已校验通过，或读不回来
/// 但写入本身没报错（fail-open）。返回 `false` 表示写入确实没成功，调用方
/// 应按写入失败处理（报 `clipboard_error` 并提示用户）。
///
/// 在 X11 上，我们持有所有权时读回是本地内存读（arboard 直接返回自己的数据），
/// 不产生额外的 X 往返；只有当所有权已被别人抢走时才会真去读一次。
pub fn write_verified(app: &AppHandle, text: &str) -> bool {
    for attempt in 1..=WRITE_ATTEMPTS {
        if let Err(e) = app.clipboard().write_text(text) {
            warn!("Clipboard write failed (attempt {attempt}): {e}");
            continue;
        }

        match app.clipboard().read_text() {
            Ok(read) if read == text => {
                if attempt > 1 {
                    info!("Clipboard write verified after rewrite");
                }
                return true;
            }
            Ok(read) => {
                // 读到了别人的内容：说明有进程刚夺走所有权，重写一次再验
                warn!(
                    "Clipboard held by someone else (attempt {attempt}): wrote {} chars, read {} chars; rewriting",
                    text.chars().count(),
                    read.chars().count()
                );
            }
            Err(e) => {
                // 读不回来不等于写失败（可能是剪贴板暂时被别的进程占用）：
                // 不重写（重写会覆盖别的内容），按原样继续
                warn!("Clipboard read-back failed (attempt {attempt}): {e}; proceeding unverified");
                return true;
            }
        }
    }

    warn!("Clipboard write could not be verified after {WRITE_ATTEMPTS} attempts");
    false
}
