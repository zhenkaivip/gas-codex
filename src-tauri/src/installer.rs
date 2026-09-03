// codex 引擎自动安装
//
// 首次启动时从 npmmirror CDN 下载 codex 二进制（国内直连，实测 9+ MB/s），
// 解压到 ~/.gas-codex/engine/，免去用户手动 npm install。
//
// 下载源（按优先级）：
//   1. npmmirror CDN:  https://cdn.npmmirror.com/packages/@openai/codex/<ver>-win32-x64/codex-<ver>-win32-x64.tgz
//   2. gh-proxy 兜底:  https://gh-proxy.com/https://github.com/openai/codex/releases/download/rust-v<ver>/codex-x86_64-pc-windows-msvc.exe.tar.gz
//
// Windows 包结构: package/vendor/x86_64-pc-windows-msvc/bin/codex.exe
//                 package/vendor/x86_64-pc-windows-msvc/...（配套资源）

use serde_json::Value;
use std::fs;

use std::path::{Path, PathBuf};

pub const CODEX_VERSION: &str = "0.153.0";

pub fn engine_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".gas-codex").join("engine")
}

pub fn codex_bin() -> PathBuf {
    if cfg!(windows) {
        engine_dir().join("bin").join("codex.exe")
    } else if cfg!(target_arch = "aarch64") {
        engine_dir().join("aarch64-pc-windows-msvc").join("bin").join("codex")
    } else {
        engine_dir().join("x86_64-pc-windows-msvc").join("bin").join("codex")
    }
}

/// 引擎是否已就位
#[tauri::command]
pub fn is_engine_installed() -> bool {
    codex_bin().exists()
}

/// 下载 URL 列表（win x64）
fn download_urls() -> Vec<String> {
    let v = CODEX_VERSION;
    vec![
        format!("https://cdn.npmmirror.com/packages/%40openai/codex/{v}-win32-x64/codex-{v}-win32-x64.tgz"),
        format!("https://registry.npmmirror.com/@openai/codex/-/codex-{v}-win32-x64.tgz"),
        format!("https://gh-proxy.com/https://github.com/openai/codex/releases/download/rust-v{v}/codex-x86_64-pc-windows-msvc.exe.tar.gz"),
    ]
}

/// 解压 .tgz（tar.gz），把 package/vendor/<triple>/ 下的内容放到 engine/
fn extract_tgz(tgz_path: &Path) -> Result<(), String> {
    let file = fs::File::open(tgz_path).map_err(|e| e.to_string())?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let dest = engine_dir();
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path: PathBuf = entry.path().map_err(|e| e.to_string())?.to_path_buf();

        let s = path.to_string_lossy().to_string();
        // 去掉 package/ 和 package/vendor/<triple>/ 前缀，直接铺到 engine/
        let rel = s
            .strip_prefix("package/vendor/x86_64-pc-windows-msvc/")
            .or_else(|| s.strip_prefix("package/vendor/aarch64-pc-windows-msvc/"))
            .map(|r| r.to_string())
            .unwrap_or_else(|| {
                // gh-proxy 的官方包结构不同：顶层就是 bin/codex.exe 等
                s.strip_prefix("package/").unwrap_or(&s).to_string()
            });

        if rel.is_empty() || rel.ends_with('/') {
            continue;
        }
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry.unpack(&out).map_err(|e| e.to_string())?;
    }

    // 标记安装完成
    fs::write(dest.join("VERSION"), CODEX_VERSION).map_err(|e| e.to_string())?;
    Ok(())
}

/// 进度事件推给前端
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    pub stage: String, // downloading | extracting | done | error
    pub percent: u32,  // 0-100
    pub message: String,
}

#[tauri::command]
pub async fn install_engine(app: tauri::AppHandle) -> Result<Value, String> {
    if is_engine_installed() {
        return Ok(serde_json::json!({"alreadyInstalled": true, "path": codex_bin().to_string_lossy()}));
    }

    use tauri::Emitter;
    let emit = |stage: &str, percent: u32, msg: &str| {
        let _ = app.emit("install-progress", InstallProgress {
            stage: stage.into(),
            percent,
            message: msg.into(),
        });
    };

    // 依次尝试下载源
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_err = String::new();
    for url in download_urls() {
        emit("downloading", 0, &format!("正在从镜像下载 codex 引擎（约 140MB）…"));
        match download_and_extract(&client, &url).await {
            Ok(()) => {
                emit("done", 100, "codex 引擎安装完成");
                return Ok(serde_json::json!({"installed": true, "path": codex_bin().to_string_lossy()}));
            }
            Err(e) => {
                last_err = format!("{url} → {e}");
                emit("downloading", 0, &format!("该镜像失败，切换下一个…"));
            }
        }
    }

    let msg = format!("所有下载源均失败：{last_err}\n可手动安装：npm install -g @openai/codex");
    emit("error", 0, &msg);
    Err(msg)
}

async fn download_and_extract(client: &reqwest::Client, url: &str) -> Result<(), String> {
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let tmp = engine_dir().join("download.tgz");
    fs::create_dir_all(engine_dir()).map_err(|e| e.to_string())?;

    // 整包下载到内存再写盘（140MB 可接受，简化实现）
    let mut resp = resp;
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
    }
    use std::io::Write;
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(&buf).map_err(|e| e.to_string())?;
    drop(file);

    let result = extract_tgz(&tmp);
    let _ = fs::remove_file(&tmp); // 清理下载包
    result
}
