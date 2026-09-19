//! 凭证门控的局域网设备发现（UDP 8765）。
//!
//! 规则见实施计划 3.3：
//! 1. 未绑定主力 → 对一切发现请求静默（隐身）；
//! 2. client_id / proof(SHA-256(token)) 任一不匹配 → 静默；
//! 3. 完全匹配 → 对请求来源单播应答，busy 只给布尔值，不泄露占用者。

use crate::server::ServerState;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::net::UdpSocket;
use std::sync::atomic::Ordering;

/// 默认端口（与 TCP 同源；实际绑定取 config.port）
const DEFAULT_PORT: u16 = 8765;
const PROTOCOL_VERSION: u32 = 1;
const RECV_BUF: usize = 1024;

#[derive(Deserialize)]
struct DiscRequest {
    #[serde(rename = "type")]
    msg_type: String,
    protocol: u32,
    client_id: String,
    proof: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct DiscResponse<'a> {
    #[serde(rename = "type")]
    msg_type: &'a str,
    protocol: u32,
    device_id: String,
    device_name: String,
    device_type: &'a str,
    port: u16,
    busy: bool,
    capabilities: DiscCapabilities,
}

#[derive(Serialize)]
struct DiscCapabilities {
    auto_paste: bool,
}

/// 启动 UDP 发现监听（常驻线程；绑定失败直接退出并记日志，不影响 TCP 主服务）
pub fn serve_discovery(state: ServerState) {
    // 发现端口必须与 TCP 端口一致：手机只知道一个端口，广播与应答都围绕它
    let port = state
        .config
        .lock()
        .map(|c| if c.port == 0 { DEFAULT_PORT } else { c.port })
        .unwrap_or(DEFAULT_PORT);
    let socket = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            error!("Discovery bind UDP {} failed: {}", port, e);
            return;
        }
    };
    info!("Discovery listener on UDP {}", port);

    let mut buf = [0u8; RECV_BUF];
    loop {
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                warn!("Discovery recv error: {}", e);
                continue;
            }
        };

        if let Some(resp) = build_response(&state, &buf[..n]) {
            if let Err(e) = socket.send_to(resp.as_bytes(), src) {
                warn!("Discovery reply to {} failed: {}", src, e);
            } else {
                info!("Discovery replied to {} ({})", src, resp.len());
            }
        }
        // 不匹配的请求：一律静默，无任何应答
    }
}

/// 解析并门控请求；通过则构造应答 JSON 字符串，否则 None。
fn build_response(state: &ServerState, payload: &[u8]) -> Option<String> {
    let req: DiscRequest = serde_json::from_slice(payload).ok()?;
    if req.msg_type != "vl_disc_req" || req.protocol != PROTOCOL_VERSION {
        return None;
    }
    if req.client_id.is_empty() || req.proof.is_empty() {
        return None;
    }

    // 锁顺序 config → session（与全局约定一致）
    let (device_id, device_name, port, busy, auto_paste) = {
        let config = state.config.lock().unwrap();
        if !config.matches_discovery(&req.client_id, &req.proof) {
            // 隐身：无主力 / client 不符 / proof 错误，全部静默
            return None;
        }
        let busy = state.session.lock().unwrap().is_some();
        (
            config.device_id.clone(),
            config.device_name.clone(),
            config.port,
            busy,
            state.auto_paste.load(Ordering::Relaxed),
        )
    };

    let resp = DiscResponse {
        msg_type: "vl_disc_resp",
        protocol: PROTOCOL_VERSION,
        device_id,
        device_name,
        device_type: "pc",
        port,
        busy,
        capabilities: DiscCapabilities { auto_paste },
    };
    serde_json::to_string(&resp).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析层：合法/畸形/版本不符/类型不符
    #[test]
    fn test_parse_request_shapes() {
        let ok = br#"{"type":"vl_disc_req","protocol":1,"client_id":"c1","proof":"abc"}"#;
        let req: DiscRequest = serde_json::from_slice(ok).unwrap();
        assert_eq!(req.msg_type, "vl_disc_req");
        assert_eq!(req.protocol, 1);
        assert_eq!(req.client_id, "c1");
        assert_eq!(req.proof, "abc");

        let cases_should_fail: &[&[u8]] = &[
            b"not json",
            br#"{"type":"vl_disc_req","protocol":2,"client_id":"c1","proof":"a"}"#,
            br#"{"type":"other","protocol":1,"client_id":"c1","proof":"a"}"#,
            br#"{"type":"vl_disc_req","protocol":1,"client_id":"","proof":"a"}"#,
            br#"{"type":"vl_disc_req","protocol":1,"proof":"a"}"#,
        ];
        for raw in cases_should_fail {
            let parsed: Result<DiscRequest, _> = serde_json::from_slice(raw);
            // 要么解析失败，要么语义检查失败
            let valid = parsed
                .map(|r| {
                    r.msg_type == "vl_disc_req"
                        && r.protocol == PROTOCOL_VERSION
                        && !r.client_id.is_empty()
                        && !r.proof.is_empty()
                })
                .unwrap_or(false);
            assert!(!valid, "should reject: {}", String::from_utf8_lossy(raw));
        }
    }

    #[test]
    fn test_response_json_shape() {
        let resp = DiscResponse {
            msg_type: "vl_disc_resp",
            protocol: 1,
            device_id: "d1".to_string(),
            device_name: "电脑".to_string(),
            device_type: "pc",
            port: 8765,
            busy: true,
            capabilities: DiscCapabilities { auto_paste: true },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"vl_disc_resp\""));
        assert!(json.contains("\"busy\":true"));
        assert!(json.contains("\"device_type\":\"pc\""));
        assert!(json.contains("\"auto_paste\":true"));
    }
}
