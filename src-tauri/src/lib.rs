// Gas Codex 应用库
//
// 模块：
// - config:  客户端配置 + codex config.toml 生成
// - bridge:  codex app-server 子进程桥接（JSON-RPC over stdio）

mod bridge;
mod config;

pub use bridge::{codex_send, codex_send_raw, codex_start, codex_stop};
pub use config::{load_config, save_config, test_connection};

use bridge::CodexState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(CodexState::default())
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            test_connection,
            codex_start,
            codex_send,
            codex_send_raw,
            codex_stop
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
