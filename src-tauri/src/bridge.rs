// codex app-server 桥接层
//
// 职责：
// 1. spawn `codex app-server --stdio` 子进程
// 2. JSONL 双向通信（JSON-RPC 2.0，无 jsonrpc 头）
// 3. 把服务端事件（item/started、turn/completed、approval 请求等）
//    通过 Tauri Channel 实时推给前端
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
    pub app: Mutex<Option<AppHandle>>, // 用于向窗口 emit 事件
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

/// 启动 codex app-server 子进程并开始读事件
#[tauri::command]
pub async fn codex_start(app: AppHandle, state: State<'_, CodexState>) -> Result<Value, String> {
    // 已在运行则直接返回
    if state.child.lock().unwrap().is_some() {
        return Ok(json!({"alreadyRunning": true}));
    }

    // 环境变量：把 API key 传给子进程（DMXAPI_KEY 是 config.toml 里 env_key 指定的名字）
    let cfg = crate::config::load_config().unwrap_or_default();

    // codex 路径解析优先级：自动安装的引擎 > 用户配置路径
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

    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "启动 codex 失败（路径: {codex_path}）: {e}\n若未安装引擎，客户端会自动下载；也可手动: npm install -g @openai/codex"
        )
    })?;

    let stdin = child.stdin.take().ok_or("无法获取 stdin")?;
    let stdout = child.stdout.take().ok_or("无法获取 stdout")?;

    *state.writer.lock().unwrap() = Some(stdin);
    *state.child.lock().unwrap() = Some(child);
    *state.app.lock().unwrap() = Some(app);

    // 后台线程：逐行读 stdout，解析 JSON-RPC，转发给前端
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(evt) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // 事件挂到全局 app handle 上 emit
            if let Some(app) = APP_HANDLE.lock().unwrap().clone() {
                let _ = app.emit("codex-event", evt);
            }
        }
    });

    Ok(json!({"started": true}))
}

// 全局 AppHandle（读线程用，State 里那份在 command 结束后不能长期持有）
static APP_HANDLE: Mutex<Option<AppHandle>> = Mutex::new(None);

/// 发送 JSON-RPC 请求（带 id，等响应）或通知（无 id）
/// 响应不在这里等——由前端监听 codex-event 事件按 id 匹配
#[tauri::command]
pub async fn codex_send(state: State<'_, CodexState>, method: String, params: Value) -> Result<Value, String> {
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let msg = json!({"method": method, "id": id, "params": params});

    let mut w = state.writer.lock().unwrap();
    let w = w.as_mut().ok_or("codex 未启动，请先调用 codex_start")?;
    w.write_all((serde_json::to_string(&msg).unwrap() + "\n").as_bytes())
        .map_err(|e| format!("写入失败: {e}"))?;
    w.flush().map_err(|e| format!("flush 失败: {e}"))?;

    // 同时登记全局 app handle（读线程要用）
    if let Some(app) = state.app.lock().unwrap().as_ref() {
        *APP_HANDLE.lock().unwrap() = Some(app.clone());
    }

    Ok(json!({"id": id}))
}

/// 发送任意 JSON（用于回传审批响应等服务端请求的 result）
#[tauri::command]
pub async fn codex_send_raw(state: State<'_, CodexState>, message: Value) -> Result<(), String> {
    let mut w = state.writer.lock().unwrap();
    let w = w.as_mut().ok_or("codex 未启动")?;
    w.write_all((serde_json::to_string(&message).unwrap() + "\n").as_bytes())
        .map_err(|e| format!("写入失败: {e}"))?;
    w.flush().map_err(|e| format!("flush 失败: {e}"))?;
    Ok(())
}

/// 停止 codex 子进程
#[tauri::command]
pub async fn codex_stop(state: State<'_, CodexState>) -> Result<(), String> {
    if let Some(mut child) = state.child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *state.writer.lock().unwrap() = None;
    *APP_HANDLE.lock().unwrap() = None;
    Ok(())
}
