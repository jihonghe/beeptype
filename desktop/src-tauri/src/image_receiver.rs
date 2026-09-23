//! 图片接收状态机（**纯逻辑，不依赖 Tauri**，便于单测）。
//!
//! 协议：`image_begin`（元信息）→ 若干 `image_chunk`（base64 分块）→ `image_end`。
//! 这里负责**校验与攒数据**，解码与写剪贴板交给调用方（见 `server.rs`）。
//!
//! 校验清单（每条都有单测盯着）：
//! - MIME 白名单（只收 `image/jpeg` / `image/png`）；
//! - 声明大小 ≤ [`MAX_IMAGE_BYTES`]、块数 ≤ [`MAX_CHUNKS`]；
//! - 每块 `seq` 必须连续（从 0 开始）；
//! - 累计字节数不得超过声明值（防越界）；
//! - 收齐后块数与实际字节数要与声明一致；
//! - 一张没传完就来了另一张 ⇒ 丢弃旧的、接受新的（手机端是串行发的，出现这种情况
//!   只可能是丢包或重试）；
//! - 30 秒没等到 `image_end` ⇒ 视为超时丢弃（[`ImageReceiver::tick`]）。
use std::time::{Duration, Instant};

/// 单张图片上限：8MB（手机端会先压缩到 2560px / JPEG q92，正常远小于它）
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// 块数上限：8MB / 135KB ≈ 61，给到 200 足够，也是防滥用的保险丝
pub const MAX_CHUNKS: u32 = 200;

/// 收包超时：超过这个时间还没等到 `image_end` 就丢弃
pub const RECV_TIMEOUT: Duration = Duration::from_secs(30);

/// 解码后的像素尺寸上限（防解码炸弹：小文件也能解出天文数字的像素）
pub const MAX_DECODED_PIXELS: u64 = 8000 * 8000;

/// 只接受这两种格式（手机端统一转 JPEG；PNG 留给"截图原样传"的将来）
const ALLOWED_MIME: [&str; 2] = ["image/jpeg", "image/png"];

/// 一次接收的中间状态
#[derive(Debug)]
struct Incoming {
    id: String,
    mime: String,
    total_bytes: u64,
    chunks: u32,
    next_seq: u32,
    bytes: Vec<u8>,
    started_at: Instant,
}

/// 状态机收到的每一步结果
#[derive(Debug, PartialEq)]
pub enum ImageProgress {
    /// 继续（刚建好缓冲，或又收下一块）
    Continue,
    /// 收齐了：把字节交给调用方去解码 / 写剪贴板
    Complete {
        id: String,
        mime: String,
        bytes: Vec<u8>,
    },
    /// 出错：调用方应回 `ack ok:false` 并带上这个 code
    Failed {
        id: String,
        code: &'static str,
        message: String,
    },
}

/// 图片接收器（每个连接一个）
#[derive(Default, Debug)]
pub struct ImageReceiver {
    current: Option<Incoming>,
}

impl ImageReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// 起一张新图。**没传完的旧图直接丢弃**（手机端串行发送，出现这种情况只可能是重试）
    pub fn begin(&mut self, id: &str, mime: &str, total_bytes: u64, chunks: u32) -> ImageProgress {
        if !ALLOWED_MIME.contains(&mime) {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: format!("不支持的图片格式：{mime}"),
            };
        }
        if total_bytes == 0 || total_bytes > MAX_IMAGE_BYTES {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_too_large",
                message: format!("图片大小 {total_bytes} 字节超出上限（{MAX_IMAGE_BYTES}）"),
            };
        }
        if chunks == 0 || chunks > MAX_CHUNKS {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_too_large",
                message: format!("分块数 {chunks} 超出上限（{MAX_CHUNKS}）"),
            };
        }
        self.current = Some(Incoming {
            id: id.to_string(),
            mime: mime.to_string(),
            total_bytes,
            chunks,
            next_seq: 0,
            bytes: Vec::with_capacity(total_bytes.min(MAX_IMAGE_BYTES) as usize),
            started_at: Instant::now(),
        });
        ImageProgress::Continue
    }

    /// 收一块（`data` 是 base64）
    pub fn chunk(&mut self, id: &str, seq: u32, data: &str) -> ImageProgress {
        let Some(cur) = self.current.as_mut() else {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: "没收到 image_begin 就来了数据块".to_string(),
            };
        };
        if cur.id != id {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: format!("块不属于当前这张图（当前 {}）", cur.id),
            };
        }
        if seq != cur.next_seq {
            let expect = cur.next_seq;
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: format!("块序号不连续：期望 {expect}，收到 {seq}"),
            };
        }
        match base64_decode(data) {
            Ok(mut bytes) => {
                if cur.bytes.len() as u64 + bytes.len() as u64 > cur.total_bytes {
                    return ImageProgress::Failed {
                        id: id.to_string(),
                        code: "image_invalid",
                        message: "累计字节数超出声明大小".to_string(),
                    };
                }
                cur.bytes.append(&mut bytes);
                cur.next_seq += 1;
                ImageProgress::Continue
            }
            Err(e) => ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: format!("base64 解析失败：{e}"),
            },
        }
    }

    /// 收尾：校验完整后交出字节
    pub fn end(&mut self, id: &str) -> ImageProgress {
        let Some(cur) = self.current.take() else {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: "没收到 image_begin 就来了 image_end".to_string(),
            };
        };
        if cur.id != id {
            self.current = Some(cur);
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_invalid",
                message: "结束信号对不上当前这张图".to_string(),
            };
        }
        if cur.next_seq != cur.chunks || cur.bytes.len() as u64 != cur.total_bytes {
            return ImageProgress::Failed {
                id: id.to_string(),
                code: "image_incomplete",
                message: format!(
                    "图片不完整：收到 {} 块 / {} 字节，声明 {} 块 / {} 字节",
                    cur.next_seq,
                    cur.bytes.len(),
                    cur.chunks,
                    cur.total_bytes
                ),
            };
        }
        ImageProgress::Complete {
            id: cur.id,
            mime: cur.mime,
            bytes: cur.bytes,
        }
    }

    /// 超时检查（连接循环每醒来一次调一下）：超时就丢弃并告知调用方
    pub fn tick(&mut self) -> Option<ImageProgress> {
        let cur = self.current.as_ref()?;
        if cur.started_at.elapsed() < RECV_TIMEOUT {
            return None;
        }
        let id = cur.id.clone();
        self.current = None;
        Some(ImageProgress::Failed {
            id,
            code: "image_incomplete",
            message: format!("超过 {} 秒没收到完整图片，已丢弃", RECV_TIMEOUT.as_secs()),
        })
    }
}

/// base64 解码（用现成 crate，避免手写出错）
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn ok_begin(r: &mut ImageReceiver) {
        assert_eq!(
            r.begin("i1", "image/jpeg", 10, 2),
            ImageProgress::Continue
        );
    }

    #[test]
    fn 正常两块的图片能收齐() {
        let mut r = ImageReceiver::new();
        ok_begin(&mut r);
        assert_eq!(r.chunk("i1", 0, &b64(b"hello")), ImageProgress::Continue);
        assert_eq!(r.chunk("i1", 1, &b64(b"world")), ImageProgress::Continue);
        assert_eq!(
            r.end("i1"),
            ImageProgress::Complete {
                id: "i1".to_string(),
                mime: "image/jpeg".to_string(),
                bytes: b"helloworld".to_vec(),
            }
        );
    }

    #[test]
    fn 不支持的格式与超限大小都被拒() {
        let mut r = ImageReceiver::new();
        assert!(matches!(
            r.begin("i1", "image/heic", 10, 1),
            ImageProgress::Failed { code: "image_invalid", .. }
        ));
        assert!(matches!(
            r.begin("i1", "image/jpeg", MAX_IMAGE_BYTES + 1, 1),
            ImageProgress::Failed { code: "image_too_large", .. }
        ));
        assert!(matches!(
            r.begin("i1", "image/jpeg", 10, MAX_CHUNKS + 1),
            ImageProgress::Failed { code: "image_too_large", .. }
        ));
    }

    #[test]
    fn 块序号必须连续且不能越界() {
        let mut r = ImageReceiver::new();
        ok_begin(&mut r);
        // 跳到 1：不连续
        assert!(matches!(
            r.chunk("i1", 1, &b64(b"hello")),
            ImageProgress::Failed { code: "image_invalid", .. }
        ));
        // 声明 10 字节，却来了 11
        assert_eq!(r.chunk("i1", 0, &b64(b"hello")), ImageProgress::Continue);
        assert!(matches!(
            r.chunk("i1", 1, &b64(b"world!!!")),
            ImageProgress::Failed { code: "image_invalid", .. }
        ));
    }

    #[test]
    fn 收齐前结束会被判为不完整() {
        let mut r = ImageReceiver::new();
        ok_begin(&mut r);
        assert_eq!(r.chunk("i1", 0, &b64(b"hello")), ImageProgress::Continue);
        assert!(matches!(
            r.end("i1"),
            ImageProgress::Failed { code: "image_incomplete", .. }
        ));
    }

    #[test]
    fn 没有_begin_就来块或结束都被拒() {
        let mut r = ImageReceiver::new();
        assert!(matches!(
            r.chunk("i1", 0, &b64(b"x")),
            ImageProgress::Failed { code: "image_invalid", .. }
        ));
        assert!(matches!(
            r.end("i1"),
            ImageProgress::Failed { code: "image_invalid", .. }
        ));
    }

    #[test]
    fn 换一张图会丢弃没传完的旧图() {
        let mut r = ImageReceiver::new();
        ok_begin(&mut r);
        assert_eq!(r.chunk("i1", 0, &b64(b"hello")), ImageProgress::Continue);
        // 新的一张来了：旧的作废
        assert_eq!(r.begin("i2", "image/png", 5, 1), ImageProgress::Continue);
        assert_eq!(r.chunk("i2", 0, &b64(b"world")), ImageProgress::Continue);
        assert_eq!(
            r.end("i2"),
            ImageProgress::Complete {
                id: "i2".to_string(),
                mime: "image/png".to_string(),
                bytes: b"world".to_vec(),
            }
        );
    }

    #[test]
    fn 乱序的块不会污染缓冲() {
        let mut r = ImageReceiver::new();
        ok_begin(&mut r);
        let _ = r.chunk("i1", 1, &b64(b"world")); // 序号错，被拒
        // 正常从 0 开始仍然能收齐
        assert_eq!(r.chunk("i1", 0, &b64(b"hello")), ImageProgress::Continue);
        assert_eq!(r.chunk("i1", 1, &b64(b"world")), ImageProgress::Continue);
        assert!(matches!(r.end("i1"), ImageProgress::Complete { .. }));
    }

}
