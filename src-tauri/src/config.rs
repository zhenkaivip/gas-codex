// 配置持久化：读写 ~/.gas-codex/config.json
// 并同步生成 ~/.codex/config.toml（codex 引擎的自定义 provider 配置）

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_codex_path")]
    pub codex_path: String,
    #[serde(default)]
    pub work_dir: String,
}

fn default_codex_path() -> String {
    // 常见安装位置；Windows 上 npm 全局装的话在 PATH 里，直接 "codex" 即可
    if cfg!(windows) {
        "codex".into()
    } else {
        "/usr/local/bin/codex".into()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: "deepseek-v4-flash-guan".into(),
            codex_path: default_codex_path(),
            work_dir: String::new(),
        }
    }
}

fn config_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".gas-codex")
}

fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

#[tauri::command]
pub fn load_config() -> Result<AppConfig, String> {
    match fs::read_to_string(config_file()) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
        Err(_) => Ok(AppConfig::default()),
    }
}

/// 保存客户端配置，并同步生成 ~/.codex/config.toml
#[tauri::command]
pub fn save_config(cfg: AppConfig) -> Result<(), String> {
    // 1. 客户端自己的配置
    fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    fs::write(config_file(), text).map_err(|e| e.to_string())?;

    // 2. codex 引擎配置（config.toml）
    let codex_home = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codex")
        });
    fs::create_dir_all(&codex_home).map_err(|e| e.to_string())?;

    let toml = format!(
        r#"# Gas Codex 客户端生成 —— 自定义模型接入
[model_providers.dmxapi]
name = "DMX API"
base_url = "{base_url}"
wire_api = "responses"
env_key = "DMXAPI_KEY"
requires_openai_auth = false
request_max_retries = 6
stream_max_retries = 10
stream_idle_timeout_ms = 180000

model_provider = "dmxapi"
model = "{model}"
"#,
        base_url = cfg.base_url.trim_end_matches('/'),
        model = cfg.model,
    );
    fs::write(codex_home.join("config.toml"), toml).map_err(|e| e.to_string())
}

/// 测试 API 连通（GET /models）
#[tauri::command]
pub async fn test_connection(cfg: AppConfig) -> Result<Vec<String>, String> {
    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if !cfg.api_key.is_empty() {
        req = req.bearer_auth(&cfg.api_key);
    }
    let resp = req.send().await.map_err(|e| format!("连接失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default())
}
