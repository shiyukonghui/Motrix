//! aria2 选项 → KGet 引擎配置映射（纯函数模块，可单元测试）
//!
//! 本模块负责把 Electron 版 `system.json`（systemKeys）与前端 `addUri` 任务级
//! options 转换为下载引擎可消费的 [`EngineOptions`]，对应迁移文档 5.3 节的
//! "aria2 选项 → KGet 配置映射" 表格（MIGRATION-TAURI.md）。
//!
//! 映射表（字段 → 来源 → KGet 对应能力）：
//! | aria2 选项（systemKeys）          | 本模块字段                | KGet 对应能力 |
//! |-----------------------------------|---------------------------|---------------|
//! | `max-connection-per-server`/`split` | `connections`             | AdvancedDownloader 并行分段连接数（内部 clamp 1~32） |
//! | `max-download-limit`              | `max_download_limit`      | 任务级限速 → Optimizer.speed_limit（TokenBucket 全局限速） |
//! | `max-overall-download-limit`      | `max_overall_download_limit` | 全局限速（KGet 无独立全局池，按任务级限速近似，取两者较小值） |
//! | `all-proxy`                       | `all_proxy`               | ProxyConfig（HTTP/HTTPS/SOCKS5，按 URL scheme 识别） |
//! | `no-proxy`                        | `no_proxy`                | KGet 1.7 无按域直连白名单，仅记录、映射时忽略 |
//! | `all-proxy-user`/`all-proxy-passwd` | `all_proxy_user`/`all_proxy_passwd` | ProxyConfig.username/password 代理认证 |
//! | `user-agent`                      | `user_agent`              | 自定义头注入（KGet 1.7 无独立 UA 方法，走 header） |
//! | `header`                          | `headers`                 | 自定义 HTTP 头（AdvancedDownloader.set_extra_headers） |
//! | `max-tries`/`retry-wait`          | `max_tries`/`retry_wait`  | 外层重试循环（KGet 内部另有每 chunk 3 次重试） |
//! | `dir`                             | `dir`                     | 输出目录（拼接到 AdvancedDownloader 输出路径） |
//! | `out`（任务级）                    | `out`                     | 输出文件名（None 时由 KGet 从 URL 推断） |
//!
//! 说明：`header` / `max-tries` / `retry-wait` / `all-proxy-user` / `all-proxy-passwd`
//! 未在 `SystemConfig` 结构体中建模，统一从 `SystemConfig.extra`（`#[serde(flatten)]`
//! 保留的未建模键）读取。

use serde_json::Value;

use crate::config::SystemConfig;

/// 引擎配置（由 aria2 systemKeys / 任务级选项映射而来，纯数据、可测试）
///
/// 注：含 `seed_ratio: Option<f64>` 等浮点字段，无法派生 `Eq`，仅实现 `PartialEq`。
#[derive(Debug, Clone, PartialEq)]
pub struct EngineOptions {
    /// 并行连接数（`max-connection-per-server` / `split` → 取两者较小值，
    /// 与 aria2 "split 且不超过 max-connection-per-server" 语义一致；
    /// KGet 内部会再次 clamp 到 1~32）
    pub connections: u32,
    /// 单任务下载限速（字节/秒，对应 `max-download-limit`；`None` 表示不限速）
    pub max_download_limit: Option<u64>,
    /// 全局下载限速（字节/秒，对应 `max-overall-download-limit`；`None` 表示不限速）
    pub max_overall_download_limit: Option<u64>,
    /// 全局代理 URL（对应 `all-proxy`，形如 `http://host:port` / `socks5://host:port`）
    pub all_proxy: Option<String>,
    /// 代理直连白名单（对应 `no-proxy`；**KGet 1.7 不支持按域直连**，仅保留字段）
    pub no_proxy: Option<String>,
    /// 代理用户名（对应 `all-proxy-user`）
    pub all_proxy_user: Option<String>,
    /// 代理密码（对应 `all-proxy-passwd`）
    pub all_proxy_passwd: Option<String>,
    /// 自定义 User-Agent（对应 `user-agent`；KGet 1.7 无独立 UA 方法，
    /// 通过自定义头注入，可能被其内置 UA 追加而非覆盖，见 kget.rs 注释）
    pub user_agent: Option<String>,
    /// 自定义 HTTP 头列表（对应 `header`，元素解析自 `"Name: Value"` 或 `"Name=Value"`）
    pub headers: Vec<(String, String)>,
    /// 最大重试次数（对应 `max-tries`，含首次尝试；aria2 中 0 = 无限重试，
    /// 本模块将其归一为较大有限值，见 [`EngineOptions::normalized_max_tries`]）
    pub max_tries: u32,
    /// 重试间隔秒数（对应 `retry-wait`）
    pub retry_wait: u64,
    /// 保存目录（对应 `dir`）
    pub dir: String,
    /// 输出文件名（任务级 `out` 选项；`None` 时由 KGet 从 URL 推断文件名）
    pub out: Option<String>,
    // ------------------------------------------------------------------
    // BitTorrent 选项（Phase 3，librqbit 集成，见 MIGRATION-TAURI.md 5.5）
    // ------------------------------------------------------------------
    /// 附加 tracker 列表（对应 `bt-tracker`，逗号分隔字符串或数组；
    /// 传给 librqbit 的 AddTorrentOptions.trackers，与种子自带 announce 并存）
    pub bt_trackers: Vec<String>,
    /// BT 监听端口（对应 `listen-port`；librqbit Session 的 TCP 监听端口，
    /// 以该端口起始的一段端口范围避免端口占用冲突）
    pub bt_listen_port: Option<u16>,
    /// 做种比率（对应 `seed-ratio`，如 2.0 表示上传达到下载量的 200% 后停止做种；
    /// librqbit 无内建做种比率控制，本字段供上层做种调度决策，当前保留）
    pub seed_ratio: Option<f64>,
    /// 做种时长（秒，对应 `seed-time`；同上，当前保留供上层调度）
    pub seed_time: Option<u32>,
    /// 下载完成后是否持续做种（对应 user.json 的 `keep-seeding`；
    /// TaskManager::add_torrent 时若任务级 options 未指定，则从 user 配置读取）
    pub keep_seeding: bool,
    /// 文件选择（对应任务级 `select-file`，如 "1,3" 表示只下载第 1、3 个文件，
    /// 传给 librqbit 的 AddTorrentOptions.only_files；仅多文件种子有意义）
    pub only_files: Option<Vec<usize>>,
}

impl EngineOptions {
    /// 由系统配置（system.json）生成引擎选项
    ///
    /// 映射来源见模块文档中的表格；`header` / `max-tries` / `retry-wait` /
    /// `all-proxy-user` / `all-proxy-passwd` 从 `SystemConfig.extra`（未建模键）读取。
    pub fn from_system_config(system: &SystemConfig) -> Self {
        // 连接数：aria2 语义为 split 与 max-connection-per-server 取较小值
        let connections = system.split.min(system.max_connection_per_server).max(1);

        // 代理认证信息（SystemConfig 未建模，存于 extra）
        let all_proxy_user = get_extra_str(system, "all-proxy-user");
        let all_proxy_passwd = get_extra_str(system, "all-proxy-passwd");

        // 自定义头：解析 system.json 的 header（字符串数组或单个字符串）
        let headers = parse_headers(system.extra.get("header"));

        // 重试：aria2 max-tries 默认 5、retry-wait 默认 0 秒
        let max_tries = get_extra_u64(system, "max-tries").map(|v| v as u32).unwrap_or(5);
        let retry_wait = get_extra_u64(system, "retry-wait").unwrap_or(0);

        Self {
            connections,
            // 限速字段各自保留原值（0 视为不限速）；kget.rs 取两者非零较小值作为实际限速
            max_download_limit: (system.max_download_limit > 0).then_some(system.max_download_limit),
            max_overall_download_limit: (system.max_overall_download_limit > 0)
                .then_some(system.max_overall_download_limit),
            all_proxy: (!system.all_proxy.is_empty()).then(|| system.all_proxy.clone()),
            no_proxy: (!system.no_proxy.is_empty()).then(|| system.no_proxy.clone()),
            all_proxy_user,
            all_proxy_passwd,
            user_agent: Some(system.user_agent.clone()),
            headers,
            max_tries,
            retry_wait,
            dir: system.dir.clone(),
            out: None,
            // BT（Phase 3）：bt-tracker 为逗号分隔 tracker 列表；listen-port 作为
            // librqbit TCP 监听端口；seed-ratio / seed-time 为 0 表示不限（保留字段）
            bt_trackers: split_tracker_list(&system.bt_tracker),
            bt_listen_port: Some(system.listen_port),
            seed_ratio: (system.seed_ratio > 0.0).then_some(system.seed_ratio),
            seed_time: (system.seed_time > 0).then_some(system.seed_time),
            keep_seeding: false,
            only_files: None,
        }
    }

    /// 把前端 `addUri` 传入的任务级 options（kebab-case 键）覆盖到本选项
    ///
    /// 任务级选项优先级高于全局（system.json）。支持键：
    /// `split` / `max-connection-per-server` / `max-download-limit` /
    /// `max-overall-download-limit` / `all-proxy` / `no-proxy` /
    /// `all-proxy-user` / `all-proxy-passwd` / `user-agent` / `header` /
    /// `max-tries` / `retry-wait` / `dir` / `out`。
    /// 限速类值允许数字或带 K/M/G 后缀字符串（如 `"1M"`）。
    pub fn apply_task_options(&mut self, options: &Value) {
        let Some(obj) = options.as_object() else {
            return;
        };
        let get_str = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_string);
        let get_num = |key: &str| -> Option<u64> {
            let v = obj.get(key)?;
            if let Some(n) = v.as_u64() {
                Some(n)
            } else {
                v.as_str().and_then(parse_size)
            }
        };

        // 连接数：任务级 split / max-connection-per-server 相互取小，并覆盖全局值
        if let Some(split) = get_num("split") {
            let max_per_server = get_num("max-connection-per-server").unwrap_or(self.connections as u64);
            self.connections = (split.min(max_per_server).max(1)) as u32;
        } else if let Some(max_per_server) = get_num("max-connection-per-server") {
            self.connections = max_per_server.max(1) as u32;
        }

        if let Some(v) = get_num("max-download-limit") {
            self.max_download_limit = (v > 0).then_some(v);
        }
        if let Some(v) = get_num("max-overall-download-limit") {
            self.max_overall_download_limit = (v > 0).then_some(v);
        }
        if let Some(v) = get_str("all-proxy") {
            self.all_proxy = (!v.is_empty()).then_some(v);
        }
        if let Some(v) = get_str("no-proxy") {
            self.no_proxy = (!v.is_empty()).then_some(v);
        }
        if let Some(v) = get_str("all-proxy-user") {
            self.all_proxy_user = (!v.is_empty()).then_some(v);
        }
        if let Some(v) = get_str("all-proxy-passwd") {
            self.all_proxy_passwd = (!v.is_empty()).then_some(v);
        }
        if let Some(v) = get_str("user-agent") {
            self.user_agent = (!v.is_empty()).then_some(v);
        }
        if obj.contains_key("header") {
            self.headers = parse_headers(obj.get("header"));
        }
        if let Some(v) = get_num("max-tries") {
            self.max_tries = v as u32;
        }
        if let Some(v) = get_num("retry-wait") {
            self.retry_wait = v;
        }
        if let Some(v) = get_str("dir") {
            self.dir = v;
        }
        if let Some(v) = get_str("out") {
            self.out = (!v.is_empty()).then_some(v);
        }
        // ---- BT 选项（Phase 3）----
        // bt-tracker：逗号分隔字符串（含空格分隔）或字符串数组
        if obj.contains_key("bt-tracker") {
            self.bt_trackers = parse_tracker_option(obj.get("bt-tracker"));
        }
        // seed-ratio：数字或字符串数字（如 2.0）
        if let Some(v) = obj.get("seed-ratio") {
            if let Some(n) = v.as_f64() {
                self.seed_ratio = Some(n);
            } else if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    self.seed_ratio = Some(n);
                }
            }
        }
        // seed-time：秒数
        if let Some(v) = obj.get("seed-time") {
            if let Some(n) = v.as_u64() {
                self.seed_time = Some(n as u32);
            } else if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<u32>() {
                    self.seed_time = Some(n);
                }
            }
        }
        // keep-seeding：布尔（字符串 "true"/"false" 或布尔值）
        if let Some(v) = obj.get("keep-seeding") {
            if let Some(b) = v.as_bool() {
                self.keep_seeding = b;
            } else if let Some(s) = v.as_str() {
                self.keep_seeding = matches!(s, "true" | "1");
            }
        }
        // select-file：字符串 "1,3"（1 起始索引）或数字数组；转为 0 起始索引传给 librqbit
        if let Some(v) = obj.get("select-file") {
            self.only_files = parse_select_file(v);
        }
    }

    /// 归一化的最大尝试次数
    ///
    /// aria2 中 `max-tries = 0` 表示无限重试；KGet 需要有限值，
    /// 这里把 0 归一化为较大有限值（100），避免无限循环。
    pub fn normalized_max_tries(&self) -> u32 {
        if self.max_tries == 0 {
            100
        } else {
            self.max_tries
        }
    }

    /// 实际生效的限速（字节/秒）：取 `max-download-limit` 与
    /// `max-overall-download-limit` 中非零的较小值（KGet 只有任务级限速池，
    /// 用较小值近似"同时满足两个上限"）
    pub fn effective_speed_limit(&self) -> Option<u64> {
        [self.max_download_limit, self.max_overall_download_limit]
            .into_iter()
            .flatten()
            .filter(|v| *v > 0)
            .min()
    }
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            connections: 16,
            max_download_limit: None,
            max_overall_download_limit: None,
            all_proxy: None,
            no_proxy: None,
            all_proxy_user: None,
            all_proxy_passwd: None,
            user_agent: None,
            headers: Vec::new(),
            max_tries: 5,
            retry_wait: 0,
            dir: ".".to_string(),
            out: None,
            bt_trackers: Vec::new(),
            bt_listen_port: None,
            seed_ratio: None,
            seed_time: None,
            keep_seeding: false,
            only_files: None,
        }
    }
}

/// 把逗号/空白分隔的 tracker 列表字符串拆成 Vec<String>（忽略空项）
///
/// system.json 的 `bt-tracker` 为逗号分隔字符串（aria2 惯例），
/// 也可能是空白分隔（Electron 版 Motrix 设置界面按行编辑后存为逗号/空格混合）。
fn split_tracker_list(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// 解析任务级 `bt-tracker` 选项（字符串 或 字符串数组）为 tracker 列表
fn parse_tracker_option(v: Option<&Value>) -> Vec<String> {
    match v {
        // 数组：["udp://t1:80", "https://t2/announce"]
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        // 字符串：逗号 / 空白分隔
        Some(Value::String(s)) => split_tracker_list(s),
        _ => Vec::new(),
    }
}

/// 解析 `select-file` 选项为 0 起始的文件索引列表（传给 librqbit only_files）
///
/// aria2 的 select-file 为 1 起始索引（逗号分隔字符串或数字数组），
/// librqbit 的文件索引为 0 起始，这里统一减 1 转换。
fn parse_select_file(v: &Value) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    let items: Vec<String> = match v {
        // 数组元素可为数字（如 [2, 4]）或字符串（如 ["2", "4"]）
        Value::Array(arr) => arr
            .iter()
            .map(|item| {
                if let Some(s) = item.as_str() {
                    s.to_string()
                } else {
                    item.to_string()
                }
            })
            .collect(),
        Value::String(s) => s.split(',').map(str::trim).map(str::to_string).collect(),
        _ => return None,
    };
    for item in items {
        if let Ok(n) = item.parse::<usize>() {
            if n >= 1 {
                out.push(n - 1);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 从 SystemConfig.extra（未建模键）读取字符串值
fn get_extra_str(system: &SystemConfig, key: &str) -> Option<String> {
    system
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 从 SystemConfig.extra 读取整数（支持数字或 "1M" 类字符串）
fn get_extra_u64(system: &SystemConfig, key: &str) -> Option<u64> {
    let v = system.extra.get(key)?;
    if let Some(n) = v.as_u64() {
        Some(n)
    } else {
        v.as_str().and_then(parse_size)
    }
}

/// 解析 aria2 `header` 选项为 (名称, 值) 列表
///
/// 支持三种形态：
/// - 字符串数组：`["Name: Value", "Name=Value"]`（aria2 system.json 常见形态）
/// - 单个字符串：`"Name: Value"`（可含多个 `\n` 分隔的条目）
/// - 其它类型：忽略
pub fn parse_headers(v: Option<&Value>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    match v {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    for line in s.lines() {
                        push_header_line(line, &mut result);
                    }
                }
            }
        }
        Some(Value::String(s)) => {
            for line in s.lines() {
                push_header_line(line, &mut result);
            }
        }
        _ => {}
    }
    result
}

/// 解析单行头（`"Name: Value"` 或 `"Name=Value"`；忽略空行与无分隔符行）
fn push_header_line(line: &str, out: &mut Vec<(String, String)>) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    // 优先按冒号分割（HTTP 头标准格式），其次按等号（部分前端存法）
    let idx = line.find(':').or_else(|| line.find('='));
    if let Some(idx) = idx {
        let name = line[..idx].trim();
        let value = line[idx + 1..].trim();
        if !name.is_empty() {
            out.push((name.to_string(), value.to_string()));
        }
    }
}

/// 解析大小字符串为字节数，支持 K/M/G/T 后缀（大小写均可，可带 B / iB）
///
/// 例：`"1024"` → 1024；`"1K"`/`"1KB"`/`"1KiB"` → 1024；
/// `"2M"` → 2097152；`"1G"` → 1073741824。
/// 非法输入返回 `None`。
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 数字部分截至第一个非数字/非小数点字符（数字内允许小数点，如 "1.5M"）
    let split_at = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num_part, suffix) = s.split_at(split_at);
    let num: f64 = num_part.trim().parse().ok()?;
    let suffix = suffix.trim().to_ascii_uppercase();
    let multiplier = match suffix.as_str() {
        "" => 1u64,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TIB" => 1024u64.pow(4),
        _ => return None,
    };
    Some((num * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemConfig;
    use std::path::Path;

    /// 便捷构造系统配置（默认值基础上调整字段）
    fn system_with(mut f: impl FnMut(&mut SystemConfig)) -> SystemConfig {
        let mut system = SystemConfig::defaults(Path::new("/tmp/motrix"));
        f(&mut system);
        system
    }

    // ------------------------------------------------------------------
    // parse_size：K/M/G 后缀解析
    // ------------------------------------------------------------------
    #[test]
    fn parse_size_supports_suffixes() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size(" 1024 "), Some(1024));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1KB"), Some(1024));
        assert_eq!(parse_size("1KiB"), Some(1024));
        assert_eq!(parse_size("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size("2MB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1.5M"), Some((1.5 * 1024.0 * 1024.0) as u64));
        // 非法输入
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("10X"), None);
        assert_eq!(parse_size("M"), None);
    }

    // ------------------------------------------------------------------
    // header 解析：数组 / 单字符串 / 分隔符
    // ------------------------------------------------------------------
    #[test]
    fn parse_headers_handles_array_and_string() {
        // 字符串数组
        let arr = serde_json::json!(["Authorization: Bearer abc", "Cookie=a=b"]);
        let headers = parse_headers(Some(&arr));
        assert_eq!(
            headers,
            vec![
                ("Authorization".to_string(), "Bearer abc".to_string()),
                ("Cookie".to_string(), "a=b".to_string()),
            ]
        );
        // 单个字符串（含换行分隔的多个头）
        let single = serde_json::json!("Referer: https://example.com\nX-Custom: v");
        let headers = parse_headers(Some(&single));
        assert_eq!(
            headers,
            vec![
                ("Referer".to_string(), "https://example.com".to_string()),
                ("X-Custom".to_string(), "v".to_string()),
            ]
        );
        // 空 / 缺失 / 非字符串
        assert!(parse_headers(Some(&serde_json::json!(""))).is_empty());
        assert!(parse_headers(None).is_empty());
        assert!(parse_headers(Some(&serde_json::json!(42))).is_empty());
        // 无效行被忽略
        let bad = serde_json::json!(["no-separator", "   "]);
        assert!(parse_headers(Some(&bad)).is_empty());
    }

    // ------------------------------------------------------------------
    // from_system_config：连接数 / 限速 / 代理 / UA / 重试 / 头
    // ------------------------------------------------------------------
    #[test]
    fn from_system_config_maps_connections_and_limits() {
        // split=8、max-connection-per-server=16 → 连接数取较小值 8
        let system = system_with(|s| {
            s.split = 8;
            s.max_connection_per_server = 16;
            s.max_download_limit = 1024 * 1024; // 1M
            s.max_overall_download_limit = 5 * 1024 * 1024; // 5M
        });
        let opts = EngineOptions::from_system_config(&system);
        assert_eq!(opts.connections, 8);
        assert_eq!(opts.max_download_limit, Some(1024 * 1024));
        assert_eq!(opts.max_overall_download_limit, Some(5 * 1024 * 1024));
        // 有效限速取较小值 1M
        assert_eq!(opts.effective_speed_limit(), Some(1024 * 1024));

        // 限速为 0（aria2 表示不限速）→ None
        let system = system_with(|s| {
            s.max_download_limit = 0;
            s.max_overall_download_limit = 0;
        });
        let opts = EngineOptions::from_system_config(&system);
        assert_eq!(opts.max_download_limit, None);
        assert_eq!(opts.max_overall_download_limit, None);
        assert_eq!(opts.effective_speed_limit(), None);
    }

    #[test]
    fn from_system_config_maps_proxy_ua_headers_and_retry() {
        let mut system = system_with(|s| {
            s.all_proxy = "http://127.0.0.1:8000".to_string();
            s.no_proxy = "localhost,127.0.0.1".to_string();
            s.user_agent = "Motrix-Test-UA".to_string();
            s.dir = "/downloads".to_string();
        });
        // 未建模键写入 extra（对应 system.json 中的 header / max-tries 等）
        system.extra.insert("header".into(), serde_json::json!(["X-Token: t1"]));
        system.extra.insert("max-tries".into(), serde_json::json!(3));
        system.extra.insert("retry-wait".into(), serde_json::json!(5));
        system.extra.insert("all-proxy-user".into(), serde_json::json!("u1"));
        system.extra.insert("all-proxy-passwd".into(), serde_json::json!("p1"));

        let opts = EngineOptions::from_system_config(&system);
        assert_eq!(opts.all_proxy.as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(opts.no_proxy.as_deref(), Some("localhost,127.0.0.1"));
        assert_eq!(opts.all_proxy_user.as_deref(), Some("u1"));
        assert_eq!(opts.all_proxy_passwd.as_deref(), Some("p1"));
        assert_eq!(opts.user_agent.as_deref(), Some("Motrix-Test-UA"));
        assert_eq!(opts.headers, vec![("X-Token".to_string(), "t1".to_string())]);
        assert_eq!(opts.max_tries, 3);
        assert_eq!(opts.retry_wait, 5);
        assert_eq!(opts.dir, "/downloads");
        assert_eq!(opts.out, None);
    }

    // ------------------------------------------------------------------
    // apply_task_options：任务级覆盖（含 "1M" 字符串限速、out 文件名）
    // ------------------------------------------------------------------
    #[test]
    fn apply_task_options_overrides_global() {
        let mut opts = EngineOptions::default();
        let options = serde_json::json!({
            "split": 4,
            "max-download-limit": "1M",
            "dir": "/tmp/tasks",
            "out": "a.zip",
            "user-agent": "Custom-UA",
            "header": ["Cookie: session=1"],
            "max-tries": 2,
            "retry-wait": 3,
        });
        opts.apply_task_options(&options);
        assert_eq!(opts.connections, 4);
        assert_eq!(opts.max_download_limit, Some(1024 * 1024));
        assert_eq!(opts.dir, "/tmp/tasks");
        assert_eq!(opts.out.as_deref(), Some("a.zip"));
        assert_eq!(opts.user_agent.as_deref(), Some("Custom-UA"));
        assert_eq!(opts.headers, vec![("Cookie".to_string(), "session=1".to_string())]);
        assert_eq!(opts.max_tries, 2);
        assert_eq!(opts.retry_wait, 3);
    }

    #[test]
    fn apply_task_options_split_and_max_connection_min() {
        let mut opts = EngineOptions::default();
        // 任务级 split 与 max-connection-per-server 取较小值
        opts.apply_task_options(&serde_json::json!({
            "split": 8,
            "max-connection-per-server": 4,
        }));
        assert_eq!(opts.connections, 4);

        // 单独设置 split
        let mut opts = EngineOptions::default();
        opts.apply_task_options(&serde_json::json!({ "split": 12 }));
        assert_eq!(opts.connections, 12);
    }

    #[test]
    fn normalized_max_tries_handles_zero() {
        let mut opts = EngineOptions::default();
        opts.max_tries = 0; // aria2 语义：无限重试
        assert_eq!(opts.normalized_max_tries(), 100);
        opts.max_tries = 5;
        assert_eq!(opts.normalized_max_tries(), 5);
    }

    // ------------------------------------------------------------------
    // BT 选项（Phase 3）：bt-tracker / seed-ratio / seed-time / keep-seeding / select-file
    // ------------------------------------------------------------------
    #[test]
    fn from_system_config_maps_bt_options() {
        let system = system_with(|s| {
            s.bt_tracker = "udp://tracker1:80, https://tracker2/announce".to_string();
            s.listen_port = 21301;
            s.seed_ratio = 2.0;
            s.seed_time = 2880;
        });
        let opts = EngineOptions::from_system_config(&system);
        // 逗号/空白分隔的 tracker 列表被拆分
        assert_eq!(
            opts.bt_trackers,
            vec![
                "udp://tracker1:80".to_string(),
                "https://tracker2/announce".to_string()
            ]
        );
        assert_eq!(opts.bt_listen_port, Some(21301));
        assert_eq!(opts.seed_ratio, Some(2.0));
        assert_eq!(opts.seed_time, Some(2880));
        assert!(!opts.keep_seeding);
        assert_eq!(opts.only_files, None);
    }

    #[test]
    fn apply_task_options_maps_bt_keys() {
        let mut opts = EngineOptions::default();
        opts.apply_task_options(&serde_json::json!({
            "bt-tracker": "udp://t1:80,udp://t2:80",
            "seed-ratio": "1.5",
            "seed-time": 600,
            "keep-seeding": "true",
            "select-file": "1,3",
        }));
        assert_eq!(opts.bt_trackers, vec!["udp://t1:80".to_string(), "udp://t2:80".to_string()]);
        assert_eq!(opts.seed_ratio, Some(1.5));
        assert_eq!(opts.seed_time, Some(600));
        assert!(opts.keep_seeding);
        // select-file 的 1 起始索引转为 0 起始（传给 librqbit only_files）
        assert_eq!(opts.only_files, Some(vec![0, 2]));

        // 数组形态的 bt-tracker / select-file
        let mut opts = EngineOptions::default();
        opts.apply_task_options(&serde_json::json!({
            "bt-tracker": ["udp://a:80", "https://b/announce"],
            "select-file": [2, 4],
        }));
        assert_eq!(opts.bt_trackers, vec!["udp://a:80".to_string(), "https://b/announce".to_string()]);
        assert_eq!(opts.only_files, Some(vec![1, 3]));
    }

    #[test]
    fn split_tracker_list_handles_blank_and_whitespace() {
        assert!(split_tracker_list("").is_empty());
        assert!(split_tracker_list(" , , ").is_empty());
        assert_eq!(
            split_tracker_list("udp://a:80 udp://b:80"),
            vec!["udp://a:80".to_string(), "udp://b:80".to_string()]
        );
    }
}
