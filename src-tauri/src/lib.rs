// Gas Codex 应用库
// run() 是入口：加载 tauri.conf.json 里配置的前端页面

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
