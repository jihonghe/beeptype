use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 配置文件结构
#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub port: u16,
    pub autostart: bool,
    /// 收到手机文本后自动模拟粘贴到光标处（默认开启）
    #[serde(default = "default_true")]
    pub auto_paste: bool,
    /// 粘贴完成后自动发送（模拟 Enter）。仅在 auto_paste=true 时生效。
    #[serde(default)]
    pub auto_send: bool,
    pub code_hash: Option<String>,
    pub code_expires_at: Option<u64>,

    /// 本机设备 ID（UUIDv4），首次运行生成；发现协议与多设备匹配用
    #[serde(default)]
    pub device_id: String,
    /// 本机设备名，默认取 hostname
    #[serde(default)]
    pub device_name: String,
    /// 唯一主力手机（持久凭证）；None = 主力位空缺
    #[serde(default)]
    pub primary: Option<PrimaryDevice>,
}

fn default_true() -> bool {
    true
}

/// 主力手机记录。token 明文只在绑定当下下发一次，此处仅存 SHA-256。
#[derive(Serialize, Deserialize, Clone)]
pub struct PrimaryDevice {
    /// 手机首启生成的 UUIDv4
    pub client_id: String,
    pub name: String,
    /// SHA-256(token) 的 hex；UDP 发现的 proof 也与此比对
    pub token_hash: String,
    pub last_ip: String,
    pub paired_at: u64,
    pub last_seen: u64,
    /// 手机硬件指纹（Android ANDROID_ID）。APP 重装不变，
    /// 用于"重装后扫同一台手机仍能识别为已绑定主力"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_id: Option<String>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(serde_json::Error),
    NoHomeDir,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "IO error: {}", e),
            ConfigError::Parse(e) => write!(f, "Parse error: {}", e),
            ConfigError::NoHomeDir => write!(f, "Cannot find home directory"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Parse(e)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8765,
            autostart: false,
            auto_paste: true,
            auto_send: false,
            code_hash: None,
            code_expires_at: None,
            device_id: String::new(),
            device_name: String::new(),
            primary: None,
        }
    }
}

/// 获取配置目录路径
fn config_dir() -> Result<PathBuf, ConfigError> {
    // 环境变量覆盖（测试用）优先
    if let Ok(custom) = std::env::var("BEETYPE_HOME") {
        return Ok(PathBuf::from(custom));
    }

    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| ConfigError::NoHomeDir)?;

    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/beetype")
    } else if cfg!(target_os = "windows") {
        home.join("AppData/Roaming/beetype")
    } else {
        home.join(".config/beetype")
    };
    Ok(dir)
}

/// 当前毫秒时间戳
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 从 /dev/urandom 填充随机字节；失败时用时间/进程信息兜底
fn fill_random(buf: &mut [u8]) {
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // fallback：非密码学场景的最差兜底（正常路径走不到）
    let mut seed = now_ms()
        .wrapping_add(std::process::id() as u64)
        .wrapping_mul(0x5851F42D4C957F2D);
    for b in buf.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed & 0xff) as u8;
    }
}

/// 生成 UUIDv4 字符串
fn new_uuid_v4() -> String {
    let mut b = [0u8; 16];
    fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// 生成 32 字节随机 token 的 hex（64 字符）
fn generate_token() -> String {
    let mut buf = [0u8; 32];
    fill_random(&mut buf);
    let mut hex = String::with_capacity(64);
    for b in buf {
        hex.push_str(&format!("{:02x}", b));
    }
    hex
}

/// 取本机设备名：HOSTNAME/COMPUTERNAME 环境变量 → /etc/hostname → 兜底
fn hostname() -> String {
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    if let Ok(content) = fs::read_to_string("/etc/hostname") {
        let name = content.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "我的电脑".to_string()
}

/// SHA-256 hex 编码
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in result {
        hex.push_str(&format!("{:02x}", byte));
    }
    hex
}

impl Config {
    /// 加载或创建配置文件
    pub fn load_or_create() -> Result<Self, ConfigError> {
        let dir = config_dir()?;
        let path = dir.join("config.json");

        let mut config = if path.exists() {
            let content = fs::read_to_string(&path)?;
            serde_json::from_str::<Config>(&content)?
        } else {
            Config::default()
        };

        // 首次运行（或旧版升级）补建本机身份，并落盘一次
        let mut dirty = false;
        if config.device_id.is_empty() {
            config.device_id = new_uuid_v4();
            dirty = true;
        }
        if config.device_name.is_empty() {
            config.device_name = hostname();
            dirty = true;
        }
        if dirty {
            config.save()?;
        }
        Ok(config)
    }

    /// 原子写入配置文件（0600 权限）
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = config_dir()?;
        let path = dir.join("config.json");
        let tmp_path = dir.join("config.json.tmp");

        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        fs::set_permissions(&dir, PermissionsExt::from_mode(0o700)).ok();

        let json = serde_json::to_string_pretty(self)?;
        fs::write(&tmp_path, &json)?;
        #[cfg(unix)]
        fs::set_permissions(&tmp_path, PermissionsExt::from_mode(0o600)).ok();

        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// 生成新配对密钥（64 位 hex 随机串），5 分钟有效。
    /// 密钥只随二维码出现，不再有人读/手输的 UI；64 字节熵避免局域网遍历猜测
    pub fn regenerate_code(&mut self) -> String {
        let code = generate_token();
        self.code_hash = Some(sha256_hex(&code));
        self.code_expires_at = Some(now_ms() + 5 * 60 * 1000);
        let _ = self.save();
        code
    }

    /// 校验配对码
    pub fn verify_code(&self, code: &str) -> bool {
        if let Some(expires) = self.code_expires_at {
            if now_ms() > expires {
                return false;
            }
        } else {
            return false;
        }
        let hash = sha256_hex(code);
        self.code_hash.as_deref() == Some(hash.as_str())
    }

    /// 配对码是否过期（测试与诊断使用）
    #[allow(dead_code)]
    pub fn is_code_expired(&self) -> bool {
        self.code_expires_at
            .map(|expires| now_ms() > expires)
            .unwrap_or(true)
    }

    /// 主力位是否已绑定
    pub fn has_primary(&self) -> bool {
        self.primary.is_some()
    }

    /// 绑定主力手机：生成新 token，持久化 client_id/name/token_hash，
    /// 返回 token 明文（仅此一次，调用方负责下发给手机）。
    /// hardware_id 可选——有则存，用于 APP 重装后识别同一物理设备。
    pub fn bind_primary(
        &mut self,
        client_id: &str,
        name: &str,
        ip: &str,
        hardware_id: Option<&str>,
    ) -> String {
        let token = generate_token();
        let ts = now_ms();
        self.primary = Some(PrimaryDevice {
            client_id: client_id.to_string(),
            name: name.to_string(),
            token_hash: sha256_hex(&token),
            last_ip: ip.to_string(),
            paired_at: ts,
            last_seen: ts,
            hardware_id: hardware_id.map(|s| s.to_string()),
        });
        let _ = self.save();
        token
    }

    /// 用主力 token 认证。成功时顺带刷新 name/last_ip/last_seen 并落盘。
    pub fn verify_token(&mut self, client_id: &str, token: &str, name: &str, ip: &str) -> bool {
        let hit = self.primary.as_ref().is_some_and(|p| {
            p.client_id == client_id && p.token_hash == sha256_hex(token)
        });
        if hit {
            if let Some(p) = self.primary.as_mut() {
                if !name.is_empty() {
                    p.name = name.to_string();
                }
                p.last_ip = ip.to_string();
                p.last_seen = now_ms();
            }
            let _ = self.save();
        }
        hit
    }

    /// 不产生持久写入的 token 校验（UDP 发现高频路径使用，避免反复写盘）
    pub fn matches_discovery(&self, client_id: &str, proof: &str) -> bool {
        self.primary
            .as_ref()
            .is_some_and(|p| p.client_id == client_id && p.token_hash == proof)
    }

    /// 解绑主力手机，旧 token 立即失效
    pub fn unbind_primary(&mut self) {
        self.primary = None;
        let _ = self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把配置目录重定向到 /tmp，避免单测写盘污染真实 ~/.config/beetype。
    /// 多线程共享同一测试目录；save 失败均被容忍，不断言落盘内容。
    fn install_test_home() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = format!("/tmp/beetype-unittest-{}", std::process::id());
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("BEETYPE_HOME", dir);
        });
    }

    #[test]
    fn test_code_verify() {
        install_test_home();
        let mut config = Config::default();
        let code = config.regenerate_code();
        assert!(config.verify_code(&code));
        assert!(!config.verify_code("000000"));
    }

    #[test]
    fn test_code_expired() {
        install_test_home();
        let mut config = Config::default();
        let code = config.regenerate_code();
        assert!(!config.is_code_expired());
        // 手动设置过期
        config.code_expires_at = Some(1); // 1970 年已过期
        assert!(config.is_code_expired());
        assert!(!config.verify_code(&code));
    }

    #[test]
    fn test_uuid_v4_shape() {
        let id = new_uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        // version/variant 位
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn test_primary_lifecycle() {
        install_test_home();
        let mut config = Config::default();
        assert!(!config.has_primary());

        let token = config.bind_primary("client-1", "小米14", "192.168.1.10");
        assert_eq!(token.len(), 64);
        assert!(config.has_primary());
        // 落盘的不是明文 token
        let saved = serde_json::to_string(&config).unwrap();
        assert!(!saved.contains(&token));

        // 正确 token + client_id
        assert!(config.verify_token("client-1", &token, "新名字", "192.168.1.20"));
        assert_eq!(config.primary.as_ref().unwrap().last_ip, "192.168.1.20");
        assert_eq!(config.primary.as_ref().unwrap().name, "新名字");
        // 错误 token / 错误 client
        assert!(!config.verify_token("client-1", "deadbeef", "x", "1.2.3.4"));
        assert!(!config.verify_token("client-2", &token, "x", "1.2.3.4"));

        // UDP proof = SHA-256(token)，正是存的 hash
        let proof = sha256_hex(&token);
        assert!(config.matches_discovery("client-1", &proof));
        assert!(!config.matches_discovery("client-1", "wrongproof"));
        assert!(!config.matches_discovery("client-2", &proof));

        config.unbind_primary();
        assert!(!config.has_primary());
        assert!(!config.verify_token("client-1", &token, "x", "1.2.3.4"));
        assert!(!config.matches_discovery("client-1", &proof));
    }

    #[test]
    fn test_persistence_roundtrip_and_migration() {
        install_test_home();
        let mut config = Config::default();
        config.device_id = new_uuid_v4();
        config.device_name = "test-pc".to_string();
        let token = config.bind_primary("cid", "phone", "10.0.0.1");

        let json = serde_json::to_string_pretty(&config).unwrap();
        let mut restored: Config = serde_json::from_str(&json).unwrap();
        assert!(restored.has_primary());
        assert_eq!(restored.device_id, config.device_id);
        assert!(restored.matches_discovery("cid", &sha256_hex(&token)));
        assert!(restored.verify_token("cid", &token, "x", "1.1.1.1"));

        // 旧版配置含 paired_device 未知字段时反序列化应被忽略而非报错
        let legacy = r#"{
            "port": 8765,
            "paired_device": {"name":"old","code_hash":"x","last_ip":"1.1.1.1","paired_at":1},
            "autostart": false,
            "auto_paste": true
        }"#;
        let migrated: Config = serde_json::from_str(legacy).unwrap();
        assert!(!migrated.has_primary());
        assert!(migrated.auto_paste);
    }
}
