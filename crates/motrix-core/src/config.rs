//! 配置读写模块：直接读写 Electron 版同名 JSON（user.json / system.json）
//!
//! 数据目录与 Electron 版 `app.getPath('userData')` 对齐：
//! - Windows: `%APPDATA%\motrix`
//! - macOS:   `~/Library/Application Support/motrix`
//! - Linux:   `~/.config/motrix`（XDG_CONFIG_HOME）
//!
//! 键名（kebab-case）与默认值与 `src/shared/configKeys.js` 的 userKeys / systemKeys
//! 以及 `src/main/core/ConfigManager.js` 的 defaults 完全一致，保证旧配置无缝迁移。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{info, warn};

/// 配置读写错误
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 无法确定系统数据目录（dirs 返回 None）
    #[error("无法确定数据目录")]
    DataDirNotFound,
    /// 配置文件读写失败
    #[error("配置文件 {path} 读写失败: {source}")]
    Io { path: PathBuf, source: io::Error },
    /// 配置文件解析失败
    #[error("配置文件 {path} 解析失败: {source}")]
    Parse { path: PathBuf, source: serde_json::Error },
    /// 配置序列化 / 反序列化失败
    #[error("配置数据不合法: {0}")]
    Serialize(#[from] serde_json::Error),
    /// 保存偏好载荷格式不合法（user / system 应为 JSON 对象）
    #[error("偏好载荷 {0} 应为 JSON 对象")]
    InvalidPayload(String),
}

/// Chrome 111 UA（与 src/shared/ua.js 的 CHROME_UA 一致）
pub const CHROME_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/111.0.0.0 Safari/537.36";

/// ngosang trackerslist 默认 tracker 源（与 src/shared/constants.js 的 CDN 地址一致）
pub const TRACKER_BEST_IP_URL_CDN: &str = "https://cdn.jsdelivr.net/gh/ngosang/trackerslist/trackers_best_ip.txt";
pub const TRACKER_BEST_URL_CDN: &str = "https://cdn.jsdelivr.net/gh/ngosang/trackerslist/trackers_best.txt";

/// 获取 Motrix 数据目录（与 Electron 版 userData 目录对齐）
pub fn data_dir() -> Result<PathBuf, ConfigError> {
    // 说明：Electron 版 app.getPath('userData') 为
    //   Windows: %APPDATA%\motrix、macOS: ~/Library/Application Support/motrix、Linux: ~/.config/motrix
    // dirs::config_dir() 在三平台恰好与之对应（Linux 下即 XDG_CONFIG_HOME / ~/.config）；
    // 而 dirs::data_dir() 在 Linux 下返回 ~/.local/share，与 Electron 版不一致，故不使用。
    let base = dirs::config_dir().ok_or(ConfigError::DataDirNotFound)?;
    Ok(base.join("motrix"))
}

/// 默认下载目录（对齐 Electron 版 app.getPath('downloads')）
fn default_download_dir() -> String {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

// ---------------------------------------------------------------------------
// 默认值辅助函数（对应 ConfigManager.js / constants.js 中的常量）
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_max_connection_per_server() -> u32 {
    64 // ENGINE_MAX_CONNECTION_PER_SERVER
}

fn default_theme() -> String {
    "auto".to_string()
}

fn default_log_level() -> String {
    "warn".to_string()
}

fn default_run_mode() -> u32 {
    1 // APP_RUN_MODE.STANDARD
}

fn default_update_channel() -> String {
    "latest".to_string()
}

fn default_locale() -> String {
    // Rust 侧无 app.getLocale() 等价 API，暂固定回退 "en-US"（Electron 版为系统 locale）
    "en-US".to_string()
}

fn default_auto_check_update() -> bool {
    cfg!(target_os = "macos") // Electron 版：is.macOS()
}

fn default_hide_app_menu() -> bool {
    cfg!(target_os = "windows") || cfg!(target_os = "linux")
}

fn default_tray_speedometer() -> bool {
    cfg!(target_os = "macos")
}

fn default_tracker_source() -> Vec<String> {
    vec![
        TRACKER_BEST_IP_URL_CDN.to_string(),
        TRACKER_BEST_URL_CDN.to_string(),
    ]
}

fn default_proxy_scope() -> Vec<String> {
    vec![
        "download".to_string(),
        "update-app".to_string(),
        "update-trackers".to_string(),
    ]
}

fn default_dht_listen_port() -> u16 {
    26701
}

fn default_listen_port() -> u16 {
    21301
}

fn default_max_concurrent_downloads() -> u32 {
    5
}

fn default_rpc_listen_port() -> u16 {
    16800 // ENGINE_RPC_PORT
}

fn default_seed_ratio() -> f64 {
    2.0
}

fn default_seed_time() -> u32 {
    2880
}

fn default_user_agent() -> String {
    CHROME_UA.to_string()
}

// ---------------------------------------------------------------------------
// UserConfig：对应 user.json
// ---------------------------------------------------------------------------

/// 协议开关配置（对应 user.json 的 protocols 对象）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProtocolsConfig {
    #[serde(default = "default_true")]
    pub magnet: bool,
    #[serde(default)]
    pub thunder: bool,
}

impl Default for ProtocolsConfig {
    fn default() -> Self {
        Self {
            magnet: true,
            thunder: false,
        }
    }
}

/// 代理配置（对应 user.json 的 proxy 对象）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProxyConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub bypass: String,
    #[serde(default = "default_proxy_scope")]
    pub scope: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enable: false,
            server: String::new(),
            bypass: String::new(),
            scope: default_proxy_scope(),
        }
    }
}

/// 用户配置（键名与 src/shared/configKeys.js 的 userKeys 一致，kebab-case）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct UserConfig {
    #[serde(default = "default_auto_check_update")]
    pub auto_check_update: bool,
    #[serde(default)]
    pub auto_hide_window: bool,
    #[serde(default = "default_true")]
    pub auto_sync_tracker: bool,
    #[serde(default)]
    pub cookie: String,
    #[serde(default = "default_true")]
    pub enable_upnp: bool,
    #[serde(default)]
    pub engine_bin_path: String,
    #[serde(default = "default_max_connection_per_server")]
    pub engine_max_connection_per_server: u32,
    #[serde(default)]
    pub favorite_directories: Vec<String>,
    #[serde(default = "default_hide_app_menu")]
    pub hide_app_menu: bool,
    #[serde(default)]
    pub history_directories: Vec<String>,
    #[serde(default)]
    pub keep_seeding: bool,
    #[serde(default)]
    pub keep_window_state: bool,
    #[serde(default)]
    pub last_check_update_time: u64,
    #[serde(default)]
    pub last_sync_tracker_time: u64,
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_true")]
    pub new_task_show_downloading: bool,
    #[serde(default)]
    pub no_confirm_before_delete_task: bool,
    #[serde(default)]
    pub open_at_login: bool,
    #[serde(default)]
    pub protocols: ProtocolsConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub resume_all_when_app_launched: bool,
    #[serde(default = "default_run_mode")]
    pub run_mode: u32,
    #[serde(default = "default_true")]
    pub show_progress_bar: bool,
    #[serde(default = "default_true")]
    pub task_notification: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_tracker_source")]
    pub tracker_source: Vec<String>,
    #[serde(default = "default_tray_speedometer")]
    pub tray_speedometer: bool,
    // —— ConfigManager.js defaults 中额外存在但不在 userKeys 的键（Electron 版会落盘）——
    #[serde(default = "default_theme")]
    pub tray_theme: String,
    #[serde(default = "default_update_channel")]
    pub update_channel: String,
    #[serde(default)]
    pub window_state: Value,
    // —— 未建模键：保存时保留，防止写回丢失 ——
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl UserConfig {
    /// 生成默认用户配置（与 ConfigManager.js initUserConfig 的 defaults 一致）
    pub fn defaults() -> Self {
        Self {
            auto_check_update: default_auto_check_update(),
            auto_hide_window: false,
            auto_sync_tracker: true,
            cookie: String::new(),
            enable_upnp: true,
            engine_bin_path: String::new(),
            engine_max_connection_per_server: default_max_connection_per_server(),
            favorite_directories: Vec::new(),
            hide_app_menu: default_hide_app_menu(),
            history_directories: Vec::new(),
            keep_seeding: false,
            keep_window_state: false,
            last_check_update_time: 0,
            last_sync_tracker_time: 0,
            locale: default_locale(),
            log_level: default_log_level(),
            new_task_show_downloading: true,
            no_confirm_before_delete_task: false,
            open_at_login: false,
            protocols: ProtocolsConfig::default(),
            proxy: ProxyConfig::default(),
            resume_all_when_app_launched: false,
            run_mode: default_run_mode(),
            show_progress_bar: true,
            task_notification: true,
            theme: default_theme(),
            tracker_source: default_tracker_source(),
            tray_speedometer: default_tray_speedometer(),
            tray_theme: default_theme(),
            update_channel: default_update_channel(),
            window_state: Map::new().into(),
            extra: Map::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SystemConfig：对应 system.json
// ---------------------------------------------------------------------------

/// 系统配置（键名与 src/shared/configKeys.js 的 systemKeys 一致，kebab-case）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SystemConfig {
    #[serde(default)]
    pub all_proxy: String,
    #[serde(default)]
    pub allow_overwrite: bool,
    #[serde(default = "default_true")]
    pub auto_file_renaming: bool,
    #[serde(default)]
    pub bt_exclude_tracker: String,
    #[serde(default)]
    pub bt_force_encryption: bool,
    #[serde(default = "default_true")]
    pub bt_load_saved_metadata: bool,
    #[serde(default = "default_true")]
    pub bt_save_metadata: bool,
    #[serde(default)]
    pub bt_tracker: String,
    // "continue" 为 Rust 关键字，用原始标识符 + 显式 rename 保证序列化键名为 "continue"
    #[serde(default = "default_true", rename = "continue")]
    pub r#continue: bool,
    #[serde(default)]
    pub dht_file_path: String,
    #[serde(default)]
    pub dht_file_path6: String,
    #[serde(default = "default_dht_listen_port")]
    pub dht_listen_port: u16,
    #[serde(default)]
    pub dir: String,
    #[serde(default = "default_true")]
    pub follow_metalink: bool,
    #[serde(default = "default_true")]
    pub follow_torrent: bool,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default = "default_max_concurrent_downloads")]
    pub max_concurrent_downloads: u32,
    #[serde(default = "default_max_connection_per_server")]
    pub max_connection_per_server: u32,
    #[serde(default)]
    pub max_download_limit: u64,
    #[serde(default)]
    pub max_overall_download_limit: u64,
    #[serde(default)]
    pub max_overall_upload_limit: u64,
    #[serde(default)]
    pub no_proxy: String,
    #[serde(default)]
    pub pause_metadata: bool,
    #[serde(default = "default_true")]
    pub pause: bool,
    #[serde(default = "default_rpc_listen_port")]
    pub rpc_listen_port: u16,
    #[serde(default)]
    pub rpc_secret: String,
    #[serde(default = "default_seed_ratio")]
    pub seed_ratio: f64,
    #[serde(default = "default_seed_time")]
    pub seed_time: u32,
    #[serde(default = "default_max_connection_per_server")]
    pub split: u32,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    // —— 未建模键：保存时保留 ——
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SystemConfig {
    /// 生成默认系统配置（与 ConfigManager.js initSystemConfig 的 defaults 一致）
    pub fn defaults(data_dir: &Path) -> Self {
        Self {
            all_proxy: String::new(),
            allow_overwrite: false,
            auto_file_renaming: true,
            bt_exclude_tracker: String::new(),
            bt_force_encryption: false,
            bt_load_saved_metadata: true,
            bt_save_metadata: true,
            bt_tracker: String::new(),
            r#continue: true,
            // dht 文件路径与 Electron 版 getDhtPath 一致：数据目录下 dht.dat / dht6.dat
            dht_file_path: data_dir.join("dht.dat").to_string_lossy().to_string(),
            dht_file_path6: data_dir.join("dht6.dat").to_string_lossy().to_string(),
            dht_listen_port: default_dht_listen_port(),
            dir: default_download_dir(),
            follow_metalink: true,
            follow_torrent: true,
            listen_port: default_listen_port(),
            max_concurrent_downloads: default_max_concurrent_downloads(),
            max_connection_per_server: default_max_connection_per_server(),
            max_download_limit: 0,
            max_overall_download_limit: 0,
            max_overall_upload_limit: 0,
            no_proxy: String::new(),
            pause_metadata: false,
            pause: true,
            rpc_listen_port: default_rpc_listen_port(),
            rpc_secret: String::new(),
            seed_ratio: default_seed_ratio(),
            seed_time: default_seed_time(),
            split: default_max_connection_per_server(),
            user_agent: default_user_agent(),
            extra: Map::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 读写函数
// ---------------------------------------------------------------------------

/// 读取用户配置；文件不存在时按默认值创建
pub fn read_user_config(data_dir: &Path) -> Result<UserConfig, ConfigError> {
    let path = data_dir.join("user.json");
    if !path.exists() {
        // 首次启动：按默认值创建文件（与 Electron 版 electron-store 行为一致）
        let config = UserConfig::defaults();
        save_user_config(data_dir, &config)?;
        info!("[Motrix] 数据目录不存在 user.json，已按默认值创建: {}", path.display());
        return Ok(config);
    }

    let content = fs::read_to_string(&path)
        .map_err(|source| ConfigError::Io { path: path.clone(), source })?;
    let mut map: Map<String, Value> = serde_json::from_str(&content)
        .map_err(|source| ConfigError::Parse { path: path.clone(), source })?;

    // 缺失键用默认值补齐（与 electron-store 的 defaults 行为一致）
    let defaults_map = serde_json::to_value(UserConfig::defaults())?
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (key, value) in defaults_map {
        map.entry(key).or_insert(value);
    }

    let config = serde_json::from_value(Value::Object(map))?;
    Ok(config)
}

/// 保存用户配置到 data_dir/user.json
pub fn save_user_config(data_dir: &Path, config: &UserConfig) -> Result<(), ConfigError> {
    let content = serde_json::to_string_pretty(config)?;
    fs::create_dir_all(data_dir)
        .map_err(|source| ConfigError::Io { path: data_dir.to_path_buf(), source })?;
    let path = data_dir.join("user.json");
    fs::write(&path, content).map_err(|source| ConfigError::Io { path, source })
}

/// 读取系统配置；文件不存在时按默认值创建
pub fn read_system_config(data_dir: &Path) -> Result<SystemConfig, ConfigError> {
    let path = data_dir.join("system.json");
    if !path.exists() {
        // 首次启动：按默认值创建文件
        let config = SystemConfig::defaults(data_dir);
        save_system_config(data_dir, &config)?;
        info!("[Motrix] 数据目录不存在 system.json，已按默认值创建: {}", path.display());
        return Ok(config);
    }

    let content = fs::read_to_string(&path)
        .map_err(|source| ConfigError::Io { path: path.clone(), source })?;
    let mut map: Map<String, Value> = serde_json::from_str(&content)
        .map_err(|source| ConfigError::Parse { path: path.clone(), source })?;

    // 缺失键用默认值补齐
    let defaults_map = serde_json::to_value(SystemConfig::defaults(data_dir))?
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (key, value) in defaults_map {
        map.entry(key).or_insert(value);
    }

    let config = serde_json::from_value(Value::Object(map))?;
    Ok(config)
}

/// 保存系统配置到 data_dir/system.json
pub fn save_system_config(data_dir: &Path, config: &SystemConfig) -> Result<(), ConfigError> {
    let content = serde_json::to_string_pretty(config)?;
    fs::create_dir_all(data_dir)
        .map_err(|source| ConfigError::Io { path: data_dir.to_path_buf(), source })?;
    let path = data_dir.join("system.json");
    fs::write(&path, content).map_err(|source| ConfigError::Io { path, source })
}

// ---------------------------------------------------------------------------
// ConfigManager：配置管理器（供 Tauri AppState 持有）
// ---------------------------------------------------------------------------

/// 配置管理器：内存持有 user/system 配置，提供读取、更新与写回能力
pub struct ConfigManager {
    /// 数据目录（user.json / system.json 所在目录）
    data_dir: PathBuf,
    /// 用户配置（user.json）
    user_config: UserConfig,
    /// 系统配置（system.json）
    system_config: SystemConfig,
}

impl ConfigManager {
    /// 初始化配置管理器：加载（或按默认值创建）user.json / system.json
    pub fn new(data_dir: PathBuf) -> Self {
        let user_config = match read_user_config(&data_dir) {
            Ok(config) => config,
            Err(e) => {
                // 配置文件损坏等极端情况：回退默认值，避免应用无法启动
                warn!("[Motrix] 加载用户配置失败，使用默认配置: {e}");
                UserConfig::defaults()
            }
        };
        let system_config = match read_system_config(&data_dir) {
            Ok(config) => config,
            Err(e) => {
                warn!("[Motrix] 加载系统配置失败，使用默认配置: {e}");
                SystemConfig::defaults(&data_dir)
            }
        };
        Self {
            data_dir,
            user_config,
            system_config,
        }
    }

    /// 数据目录
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 用户配置引用
    pub fn user_config(&self) -> &UserConfig {
        &self.user_config
    }

    /// 系统配置引用
    pub fn system_config(&self) -> &SystemConfig {
        &self.system_config
    }

    /// 用户配置序列化为 JSON
    pub fn user_config_json(&self) -> Value {
        serde_json::to_value(&self.user_config).unwrap_or(Value::Null)
    }

    /// 系统配置序列化为 JSON
    pub fn system_config_json(&self) -> Value {
        serde_json::to_value(&self.system_config).unwrap_or(Value::Null)
    }

    /// 合并用户配置补丁并写盘（未在补丁中的键与未建模的 extra 键均保留）
    pub fn update_user_config(&mut self, patch: &Map<String, Value>) -> Result<(), ConfigError> {
        let mut map = serde_json::to_value(&self.user_config)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (key, value) in patch {
            map.insert(key.clone(), value.clone());
        }
        // 反序列化回结构体：类型校验 + 未知键进入 extra
        self.user_config = serde_json::from_value(Value::Object(map))?;
        save_user_config(&self.data_dir, &self.user_config)
    }

    /// 合并系统配置补丁并写盘（未在补丁中的键与未建模的 extra 键均保留）
    pub fn update_system_config(&mut self, patch: &Map<String, Value>) -> Result<(), ConfigError> {
        let mut map = serde_json::to_value(&self.system_config)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (key, value) in patch {
            map.insert(key.clone(), value.clone());
        }
        self.system_config = serde_json::from_value(Value::Object(map))?;
        save_system_config(&self.data_dir, &self.system_config)
    }

    /// 应用偏好保存（等价 Electron 版 savePreference：user / system 分区写回）
    ///
    /// payload 形如 `{ "user": {...}, "system": {...} }`，user / system 均可省略。
    pub fn apply_preference(
        &mut self,
        user: Option<&Value>,
        system: Option<&Value>,
    ) -> Result<(), ConfigError> {
        if let Some(value) = system {
            let patch = value
                .as_object()
                .ok_or_else(|| ConfigError::InvalidPayload("system".to_string()))?;
            self.update_system_config(patch)?;
        }
        if let Some(value) = user {
            let patch = value
                .as_object()
                .ok_or_else(|| ConfigError::InvalidPayload("user".to_string()))?;
            self.update_user_config(patch)?;
        }
        Ok(())
    }

    /// JSON-RPC 监听端口（供后续 motrix-rpc 服务读取）
    pub fn rpc_listen_port(&self) -> u16 {
        self.system_config.rpc_listen_port
    }

    /// JSON-RPC 密钥（供后续 motrix-rpc 服务读取）
    pub fn rpc_secret(&self) -> &str {
        &self.system_config.rpc_secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建带唯一后缀的临时目录（测试后清理）
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "motrix-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 清理临时目录
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn defaults_use_kebab_case_keys() {
        // 用户配置默认值应输出 kebab-case 键（与 configKeys.js 的 userKeys 一致）
        let user = UserConfig::defaults();
        let user_obj = serde_json::to_value(&user).unwrap();
        let user_obj = user_obj.as_object().unwrap();
        assert!(user_obj.contains_key("auto-check-update"));
        assert!(user_obj.contains_key("engine-max-connection-per-server"));
        assert!(user_obj.contains_key("new-task-show-downloading"));
        assert!(user_obj.contains_key("tray-speedometer"));
        assert_eq!(user_obj["theme"], "auto");
        assert_eq!(user_obj["run-mode"], 1);

        // 系统配置默认值（与 configKeys.js 的 systemKeys / ConfigManager.js 一致）
        let system = SystemConfig::defaults(Path::new("/tmp/motrix"));
        let system_obj = serde_json::to_value(&system).unwrap();
        let system_obj = system_obj.as_object().unwrap();
        assert!(system_obj.contains_key("max-concurrent-downloads"));
        assert!(system_obj.contains_key("rpc-listen-port"));
        assert!(system_obj.contains_key("rpc-secret"));
        // "continue" 为 Rust 关键字，序列化键名必须还原为 "continue"
        assert!(system_obj.contains_key("continue"));
        assert_eq!(system_obj["continue"], true);
        assert_eq!(system_obj["rpc-listen-port"], 16800);
        assert_eq!(system_obj["max-connection-per-server"], 64);
        assert_eq!(system_obj["dir"], default_download_dir());
    }

    #[test]
    fn read_creates_default_files_when_missing() {
        let dir = temp_dir();
        // 文件不存在时应按默认值创建 user.json / system.json
        let user = read_user_config(&dir).unwrap();
        let system = read_system_config(&dir).unwrap();
        assert!(dir.join("user.json").exists());
        assert!(dir.join("system.json").exists());
        assert_eq!(user.theme, "auto");
        assert_eq!(system.max_concurrent_downloads, 5);
        assert_eq!(system.rpc_listen_port, 16800);
        cleanup(&dir);
    }

    #[test]
    fn update_merges_and_preserves_extra_keys() {
        let dir = temp_dir();
        let mut manager = ConfigManager::new(dir.clone());

        // 更新部分键：只影响补丁中的键，其余默认值保留
        let patch = serde_json::json!({
            "user": { "theme": "dark", "some-custom-key": 42 },
            "system": { "max-concurrent-downloads": 10 }
        });
        manager
            .apply_preference(patch.get("user"), patch.get("system"))
            .unwrap();

        // 用户配置：theme 已更新、其它默认值保留、未知键进入 extra 且不丢失
        assert_eq!(manager.user_config().theme, "dark");
        assert_eq!(manager.user_config().log_level, "warn");
        assert_eq!(
            manager.user_config().extra.get("some-custom-key"),
            Some(&serde_json::json!(42))
        );
        // 系统配置
        assert_eq!(manager.system_config().max_concurrent_downloads, 10);
        assert_eq!(manager.system_config().rpc_listen_port, 16800);

        // 写盘后可重新读取到更新结果（重启持久化）
        let reloaded = read_user_config(&dir).unwrap();
        assert_eq!(reloaded.theme, "dark");
        assert_eq!(reloaded.extra.get("some-custom-key"), Some(&serde_json::json!(42)));
        cleanup(&dir);
    }
}
