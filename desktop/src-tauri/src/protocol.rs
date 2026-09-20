use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// 最大消息大小（256KB）
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// 设备能力位（hello_ack / 发现应答中携带）
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Capabilities {
    #[serde(default)]
    pub auto_paste: bool,
}

/// 消息类型
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum Message {
    /// 手机端 → PC：握手，配对码与持久 token 二选一；
    /// client_id 为手机首启生成的 UUIDv4。旧客户端缺省字段按默认值兼容。
    #[serde(rename = "hello")]
    Hello {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// 临时会话续连凭证（与 code/token 三选一）。临时手机被系统杀掉进程后，
        /// 凭它在窗口期内免扫码续接；PC 端只在内存里存它的 SHA-256。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
        #[serde(default)]
        client_id: String,
        #[serde(default)]
        device_name: String,
        /// 手机硬件指纹（Android ANDROID_ID）。重装 APP 后不变，
        /// 用于 PC 端重新识别"这台手机我之前绑定过"，绕过 client_id 丢失问题。
        /// 旧客户端不发此字段，按 None 处理。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hardware_id: Option<String>,
    },

    /// PC → 手机端：握手响应。
    /// status: "ok" | "occupied" | "bad_code" | "unauthorized" | "cooldown"
    /// session: "primary" | "transient"（仅成功时出现）
    /// token: 仅新绑定主力的那一次响应出现，明文仅此下发一次
    #[serde(rename = "hello_ack")]
    HelloAck {
        ok: bool,
        #[serde(default = "ack_status_default")]
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// 临时会话的续连凭证，仅"这次码配对新接入的临时会话"下发一次（明文只此一次）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_token: Option<String>,
        /// 续连窗口时长（秒）。临时会话的每次握手响应都会带上，手机据此刷新本地到期时间
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_expires_in: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Capabilities>,
    },

    /// 手机端 → PC：文字消息
    #[serde(rename = "text")]
    Text {
        id: String,
        text: String,
        ts: u64,
    },

    /// PC → 手机端：文字确认
    #[serde(rename = "ack")]
    Ack {
        id: String,
        ok: bool,
        code: Option<String>,
        pasted: bool,
        srv_recv_ms: u64,
    },

    /// 手机端 → PC：心跳
    #[serde(rename = "ping")]
    Ping,

    /// PC → 手机端：心跳响应
    #[serde(rename = "pong")]
    Pong,

    /// 手机端 → PC：主力机请求接管被占用的电脑
    #[serde(rename = "takeover")]
    Takeover,

    /// PC → 手机端：接管结果
    #[serde(rename = "takeover_ack")]
    TakeoverAck { ok: bool },

    /// 手机端 → PC：主动断开
    #[serde(rename = "bye")]
    Bye,

    /// PC → 手机端：断开已受理
    #[serde(rename = "bye_ack")]
    ByeAck,

    /// PC → 手机端：会话被对端结束（被踢等）。
    /// reason: "owner_takeover" | "owner_disconnect" | "replaced"
    /// 手机端不区分原因，统一展示中性文案。
    #[serde(rename = "session_ended")]
    SessionEnded { reason: String },

    /// PC → 手机端：主力已解绑，当前连接降级为临时（会话保持，不断开）。
    /// 手机端收到后应删除本地凭证并把当前会话标记为临时连接。
    #[serde(rename = "session_downgraded")]
    SessionDowngraded,
}

fn ack_status_default() -> String {
    "ok".to_string()
}

impl Message {
    /// 编码为 JSON 行（以 \n 结尾）
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        let json = serde_json::to_string(self)?;
        Ok(format!("{}\n", json))
    }

    /// 从一行 JSON 解析
    pub fn decode_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim())
    }

    /// 构造码/ token 握手（测试与客户端对称实现参考）
    #[allow(dead_code)]
    pub fn hello_with_code(code: &str, client_id: &str, device_name: &str) -> Self {
        Message::Hello {
            code: Some(code.to_string()),
            token: None,
            resume: None,
            client_id: client_id.to_string(),
            device_name: device_name.to_string(),
            hardware_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn hello_with_token(token: &str, client_id: &str, device_name: &str) -> Self {
        Message::Hello {
            code: None,
            token: Some(token.to_string()),
            resume: None,
            client_id: client_id.to_string(),
            device_name: device_name.to_string(),
            hardware_id: None,
        }
    }

    /// 构造临时会话的续连握手（测试与客户端对称实现参考）
    #[allow(dead_code)]
    pub fn hello_with_resume(resume: &str, client_id: &str, device_name: &str) -> Self {
        Message::Hello {
            code: None,
            token: None,
            resume: Some(resume.to_string()),
            client_id: client_id.to_string(),
            device_name: device_name.to_string(),
            hardware_id: None,
        }
    }

    /// 构造成功的握手响应
    /// `resume`: (续连凭证明文, 窗口秒数)。仅临时会话用；主力传 None。
    pub fn hello_ack_ok(
        session: &str,
        device_id: &str,
        device_name: &str,
        token: Option<String>,
        resume: Option<(String, u64)>,
        caps: Capabilities,
    ) -> Self {
        let (resume_token, resume_expires_in) = match resume {
            Some((t, secs)) => (Some(t), Some(secs)),
            None => (None, None),
        };
        Message::HelloAck {
            ok: true,
            status: "ok".to_string(),
            error: None,
            session: Some(session.to_string()),
            device_id: Some(device_id.to_string()),
            device_name: Some(device_name.to_string()),
            device_type: Some("pc".to_string()),
            token,
            resume_token,
            resume_expires_in,
            capabilities: Some(caps),
        }
    }

    /// 给握手响应补上续连窗口时长（续连成功时用：凭证不重发，只更新本地到期时间）
    pub fn with_resume_expires_in(mut self, secs: u64) -> Self {
        if let Message::HelloAck {
            resume_expires_in, ..
        } = &mut self
        {
            *resume_expires_in = Some(secs);
        }
        self
    }

    /// 构造失败的握手响应
    pub fn hello_ack_err(status: &str, error: &str) -> Self {
        Message::HelloAck {
            ok: false,
            status: status.to_string(),
            error: Some(error.to_string()),
            session: None,
            device_id: None,
            device_name: None,
            device_type: None,
            token: None,
            resume_token: None,
            resume_expires_in: None,
            capabilities: None,
        }
    }
}

/// 当前毫秒时间戳
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 生成消息 ID（时间戳 + 随机后缀）
#[allow(dead_code)]
pub fn generate_id() -> String {
    let ms = now_ms();
    let rand = {
        let mut buf = [0u8; 4];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read(&mut buf);
        }
        u32::from_le_bytes(buf)
    };
    format!("{}-{:08x}", ms, rand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_hello() {
        let msg = Message::hello_with_code("123456", "cid-1", "Pixel 8");
        let encoded = msg.encode().unwrap();
        assert!(encoded.ends_with('\n'));
        let decoded = Message::decode_line(&encoded).unwrap();
        assert_eq!(msg, decoded);

        let msg2 = Message::hello_with_token("abc", "cid-1", "Pixel 8");
        let decoded2 = Message::decode_line(&msg2.encode().unwrap()).unwrap();
        assert_eq!(msg2, decoded2);
    }

    #[test]
    fn test_legacy_hello_compatible() {
        // 旧版客户端只发 code + device_name，缺 client_id，必须能解析
        let old = r#"{"type":"hello","code":"123456","device_name":"OldPhone"}"#;
        let decoded = Message::decode_line(old).unwrap();
        match decoded {
            Message::Hello {
                code,
                token,
                resume,
                client_id,
                device_name,
                hardware_id,
            } => {
                assert_eq!(code.as_deref(), Some("123456"));
                assert!(token.is_none());
                assert!(resume.is_none());
                assert_eq!(client_id, "");
                assert_eq!(device_name, "OldPhone");
                // 旧客户端不发硬件指纹
                assert!(hardware_id.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_legacy_hello_ack_compatible() {
        // 旧版 PC 的 ack 只有 ok/error，新结构必须能解析
        let old = r#"{"type":"hello_ack","ok":true,"error":null}"#;
        let decoded = Message::decode_line(old).unwrap();
        match decoded {
            Message::HelloAck { ok, status, session, .. } => {
                assert!(ok);
                assert_eq!(status, "ok");
                assert!(session.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_hello_ack_constructors() {
        let ok = Message::hello_ack_ok(
            "primary",
            "dev-1",
            "pc",
            Some("tok".to_string()),
            None,
            Capabilities { auto_paste: true },
        );
        let json = ok.encode().unwrap();
        assert!(json.contains("\"session\":\"primary\""));
        assert!(json.contains("\"device_type\":\"pc\""));
        assert!(json.contains("\"token\":\"tok\""));

        let err = Message::hello_ack_err("occupied", "电脑使用中");
        let json = err.encode().unwrap();
        assert!(json.contains("\"status\":\"occupied\""));
        assert!(!json.contains("session"));
    }

    #[test]
    fn test_new_messages_roundtrip() {
        for msg in [
            Message::Takeover,
            Message::TakeoverAck { ok: true },
            Message::Bye,
            Message::ByeAck,
            Message::SessionEnded {
                reason: "owner_takeover".to_string(),
            },
            Message::SessionDowngraded,
        ] {
            let encoded = msg.encode().unwrap();
            let decoded = Message::decode_line(&encoded).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn test_encode_decode_text() {
        let msg = Message::Text {
            id: "123-abc".to_string(),
            text: "你好世界 Hello World 🎉".to_string(),
            ts: now_ms(),
        };
        let encoded = msg.encode().unwrap();
        let decoded = Message::decode_line(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_encode_decode_ping() {
        let msg = Message::Ping;
        let encoded = msg.encode().unwrap();
        let decoded = Message::decode_line(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    /// 临时会话续连握手：请求与响应的往返编解码（含续连字段）
    #[test]
    fn test_transient_resume_handshake_roundtrip() {
        let hello = Message::hello_with_resume("f".repeat(64).as_str(), "cid-1", "临时手机");
        let line = hello.encode().unwrap();
        assert!(line.contains("\"resume\""));
        match Message::decode_line(&line).unwrap() {
            Message::Hello { resume, code, token, .. } => {
                assert_eq!(resume.as_deref(), Some("f".repeat(64).as_str()));
                assert!(code.is_none() && token.is_none());
            }
            _ => panic!("wrong variant"),
        }

        // 续连成功：下发窗口时长（不重发凭证）
        let ack = Message::hello_ack_ok(
            "transient",
            "dev-1",
            "pc",
            None,
            None,
            Capabilities { auto_paste: true },
        )
        .with_resume_expires_in(300);
        let json = ack.encode().unwrap();
        assert!(json.contains("\"resume_expires_in\":300"));
        assert!(!json.contains("resume_token"));
        match Message::decode_line(&json).unwrap() {
            Message::HelloAck { resume_expires_in, .. } => {
                assert_eq!(resume_expires_in, Some(300));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_decode_invalid() {
        let result = Message::decode_line("not json at all");
        assert!(result.is_err());
    }
}
