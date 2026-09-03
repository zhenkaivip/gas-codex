// codex app-server 桥接层
//
// 职责：
// 1. spawn `codex app-server --stdio` 子进程
// 2. JSONL 双向通信（JSON-RPC 2.0，无 jsonrpc 头）
// 3. 把服务端事件（item/started、turn/completed、approval 请求等）
//    通过 Tauri 事件实时推给前端
// 4. 进程树清理（kill 引擎时连带它起的 shell 子进程）
// 5. 引擎崩溃检测与通知（前端可自动重启）
//
// 协议参考：/data/codex/codex-rs/app-server/README.md

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};

// ---------- 全局状态 ----------

pub struct CodexState {
    pub child: Mutex<Option<Child>>,
    pub writer: Mutex<Option<std::process::ChildStdin>>,
    pub next_id: AtomicU64,
    pub app: Mutex<Option<AppHandle>>,
}

impl Default for CodexState {
    fn default() -> Self {
        Self {
            child: Mutex::new(None),
            writer: Mutex::new(None),
            next_id: AtomicU64::new(1),
            app: Mutex::new(None),
        }
    }
}

// 全局 AppHandle（读线程用）
static APP_HANDLE: Mutex<Option<AppHandle>> = Mutex::new(None);

/// Windows 上杀整棵进程树（taskkill /T）；Unix 上杀进程组
fn kill_process_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        // /T 连带子进程，/F 强制
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // 优雅起见先 SIGTERM（codex 自己会清理子进程），短等后强杀
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// 启动 codex app-server 子进程并开始读事件
#[tauri::command]
pub async fn codex_start(app: AppHandle, state: State<'_, CodexState>) -> Result<Value, String> {
    // 已在运行则直接返回（try_wait 探测是否退出过）
    {
        let mut guard = state.child.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => { /* 已退出，走下面的重新启动流程 */ }
                Ok(None) => return Ok(json!({"alreadyRunning": true})),
            }
        }
    }

    let cfg = crate::config::load_config().unwrap_or_default();

    // codex 路径解析：自动安装的引擎 > 用户配置路径
    let engine_bin = crate::installer::codex_bin();
    let codex_path = if crate::installer::is_engine_installed() {
        engine_bin.to_string_lossy().to_string()
    } else {
        cfg.codex_path.clone()
    };

    let mut cmd = Command::new(&codex_path);
    cmd.args(["app-server", "--stdio"])
        .env("DMXAPI_KEY", &cfg.api_key)
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // Unix 上建立新进程组，方便整组收割
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "启动 codex 失败（路径: {codex_path}）: {e}\n若未安装引擎，客户端会自动下载；也可手动: npm install -g @openai/codex"
        )
    })?;

    let stdin = child.stdin.take().ok_or("无法获取 stdin")?;
    let stdout = child.stdout.take().ok_or("无法获取 stdout")?;

    *state.writer.lock().unwrap() = Some(stdin);
    *state.child.lock().unwrap() = Some(child);
    *state.app.lock().unwrap() = Some(app.clone());
    *APP_HANDLE.lock().unwrap() = Some(app);

    // 读线程：读事件 + 检测进程退出
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(evt) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(app) = APP_HANDLE.lock().unwrap().clone() {
                let _ = app.emit("codex-event", evt);
            }
        }
        // 流结束 = 引擎进程退出（崩溃或被杀）
        // 通知前端；下一次 codex_start 的状态检测会自动重新拉起
        if let Some(app) = APP_HANDLE.lock().unwrap().clone() {
            let _ = app.emit(
                "codex-exit",
                json!({"message": "codex 引擎进程已退出"}),
            );
        }
    });

    Ok(json!({"started": true}))
}

/// 发送 JSON-RPC 请求（带 id）或通知（无 id 由 params 判断——这里统一带 id）
#[tauri::command]
pub async fn codex_send(state: State<'_, CodexState>, method: String, params: Value) -> Result<Value, String> {
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    // "initialized" 等通知类方法无 id
    let msg = if method == "initialized" {
        json!({"method": method, "params": params})
    } else {
        json!({"method": method, "id": id, "params": params})
    };

    let mut w = state.writer.lock().unwrap();
    let w = w.as_mut().ok_or("codex 未启动，请先调用 codex_start")?;
    w.write_all((serde_json::to_string(&msg).unwrap() + "\n").as_bytes())
        .map_err(|e| format!("写入失败: {e}（引擎可能已退出）"))?;
    w.flush().map_err(|e| format!("flush 失败: {e}"))?;

    if let Some(app) = state.app.lock().unwrap().as_ref() {
        *APP_HANDLE.lock().unwrap() = Some(app.clone());
    }

    Ok(json!({"id": id}))
}

/// 发送任意 JSON（审批响应回传等）
#[tauri::command]
pub async fn codex_send_raw(state: State<'_, CodexState>, message: Value) -> Result<(), String> {
    let mut w = state.writer.lock().unwrap();
    let w = w.as_mut().ok_or("codex 未启动")?;
    w.write_all((serde_json::to_string(&message).unwrap() + "\n").as_bytes())
        .map_err(|e| format!("写入失败: {e}"))?;
    w.flush().map_err(|e| format!("flush 失败: {e}"))?;
    Ok(())
}

/// 停止 codex 子进程（连带进程树）
#[tauri::command]
pub async fn codex_stop(state: State<'_, CodexState>) -> Result<(), String> {
    if let Some(mut child) = state.child.lock().unwrap().take() {
        kill_process_tree(&mut child);
    }
    *state.writer.lock().unwrap() = None;
    *APP_HANDLE.lock().unwrap() = None;
    Ok(())
}
