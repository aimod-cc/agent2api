//! 软件更新 —— GitHub Release 检测与安装包下载（对照 workbuddy-update.mjs）。
//!
//! ── 为什么检测/下载在后端而不是 Tauri 壳 ────────────────────
//! 壳侧 reqwest 为省掉 TLS 依赖关掉了默认特性（只能访问本机网关的明文 HTTP），
//! 发不出 GitHub 的 HTTPS 请求。因此这里负责联网，壳只做两件事：
//! 把「当前版本」传进来比较、把下载好的安装包跑起来。
//!
//! ── 出口策略：直连优先，失败后借 Clash 混合端口兜一次 ────────
//! 国内访问 GitHub 常需代理，但用户不一定给账号配了代理 ——
//! 直连能通就别打扰，通不了再借 Clash 的混合端口重试。候选顺序与
//! Node 版 `resolveEgressCandidates` 一致：`[直连, Clash 混合端口?]`。
//! **只有网络层失败才切出口**：HTTP 状态码类错误直接交给调用方判断，
//! 否则 404（仓库没有 Release）会被误当成「需要换代理」而白试一轮。
//!
//! ── 文件分工 ────────────────────────────────────────────────
//!   mod.rs          句柄、下载状态机、进度/取消、check 的响应组装（本文件）
//!   version.rs      版本比较、域名白名单、资产挑选、文件名安全化（纯函数）
//!   client.rs       出网候选解析、GitHub 请求头、带出口重试的 fetch
//!
//! ── 与 Node 版的一处结构性差异：下载任务的所有权 ─────────────
//! Node 用一个模块级 `task` 变量（同一时刻只允许一个下载）；Rust 侧同一份状态
//! 放句柄内部的 `Mutex<Task>`，**下载字节数与进度也在同一把锁里更新**。
//! 硬约束「持锁不做网络请求、不 await」因此落在：读响应流时按 chunk 加锁更新
//! 计数 → 放锁 → 写文件。写文件是同步 IO，在 tokio 的工作线程上执行
//! （reqwest 的 bytes_stream 本身就是 async，每条 chunk 的落盘量很小）。

mod client;
mod version;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::server::logging;

pub use version::{
    assert_downloadable, compare_versions, installer_kind, pick_installer, safe_file_name,
    UpdateError, DEFAULT_REPO, GITHUB_API, MAX_INSTALLER_BYTES,
};

/// 检测与下载建连阶段的超时（Node 版 REQUEST_TIMEOUT_MS）。
/// 下载本身不设总超时（长下载会被掐断），只在建连阶段给足时间。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 请求 GitHub 的 UA（Node 版字面量）
const USER_AGENT: &str = "workbuddy-local-proxy";

/// 本应用的当前版本号（编译期取自 Cargo.toml，发布流程与 tauri.conf.json
/// 同步更新）。壳的 `checkUpdate` 命令用的是运行时 package_info —— 两者常态
/// 一致；后端定时检查（scheduled_tasks）拿不到 Tauri 句柄，用这一份自足。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ─── 下载任务状态 ───────────────────────────────────────────

/// 下载任务（对应 Node 版闭包里的 `task` 对象）。
#[derive(Clone, Debug)]
struct Task {
    active: bool,
    done: bool,
    canceled: bool,
    error: Option<String>,
    filename: String,
    /// 落盘路径；取消或失败后置 None（与 Node 的 `current.path = null` 一致）
    path: Option<String>,
    received: u64,
    total: u64,
}

impl Task {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("active".to_string(), Value::Bool(self.active));
        map.insert("done".to_string(), Value::Bool(self.done));
        map.insert("canceled".to_string(), Value::Bool(self.canceled));
        map.insert(
            "error".to_string(),
            self.error.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert("filename".to_string(), Value::String(self.filename.clone()));
        map.insert(
            "path".to_string(),
            self.path.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert("received".to_string(), Value::from(self.received));
        map.insert("total".to_string(), Value::from(self.total));
        // percent：总量未知时为 0；已知时向上取整到 100 封顶
        // （Node: `task.total > 0 ? Math.min(100, Math.round(received/total*100)) : 0`）
        let percent = if self.total > 0 {
            let raw = (self.received as f64 / self.total as f64 * 100.0).round() as i64;
            raw.clamp(0, 100)
        } else {
            0
        };
        map.insert("percent".to_string(), Value::from(percent));
        Value::Object(map)
    }
}

/// 管理器内部状态：下载任务、取消标志、最近一次拉到的 Release、仓库名。
struct Inner {
    task: Option<Task>,
    /// 与任务一一对应的取消标志（Node 用 AbortController，Rust 用原子标志）
    cancel_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 最近一次拉到的 Release（内存缓存）
    latest: Option<Value>,
    /// 最近一次「检查更新」的结果（内存缓存）。
    ///
    /// 只在 check 成功后写入：定时任务（scheduled_tasks 的「软件版本检查」）
    /// 到点跑一次 check，前端轮询 `/api/update/status` 读这里来亮侧栏徽标，
    /// 不必自己再打一遍 GitHub（匿名限额 60 次/小时，双端各查一遍就翻倍了）。
    last_check: Option<Value>,
    repository: String,
    download_dir: PathBuf,
}

/// 更新管理器句柄：内部一把锁 + Clone（与 auto_checkin / models 同构）。
#[derive(Clone)]
pub struct UpdateManager {
    inner: Arc<Mutex<Inner>>,
}

impl UpdateManager {
    /// 构造管理器。`directory` 是配置目录（下载落在 `{directory}/updates`）。
    ///
    /// 仓库解析顺序与 Node 版一致：参数 > `WORKBUDDY_UPDATE_REPO` > DEFAULT_REPO
    /// （`String(repo || env || DEFAULT).trim()`）。
    pub fn new(directory: PathBuf) -> Self {
        let repository = std::env::var("WORKBUDDY_UPDATE_REPO")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_REPO.to_string());
        Self {
            inner: Arc::new(Mutex::new(Inner {
                task: None,
                cancel_flag: None,
                latest: None,
                last_check: None,
                repository,
                download_dir: directory.join("updates"),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 仓库名（`owner/repo`）
    pub fn repository(&self) -> String {
        self.lock().repository.clone()
    }

    /// 下载目录（`{config_dir}/updates`）—— 与壳侧 `update::download_dir()` 同源
    pub fn download_dir(&self) -> PathBuf {
        self.lock().download_dir.clone()
    }

    // ─── 检查新版本 ─────────────────────────────────────────

    /// 检查是否有新版本（对应 Node 版 check）。
    ///
    /// `current_version` 由桌面端传入（后端不知道自己被哪个壳打包）；
    /// 缺省时只回报最新版本，不做「是否有更新」的判断。
    /// 成功的结果缓存进 `last_check`（见 Inner 字段说明）。
    pub async fn check(&self, current_version: &str) -> Result<Value, UpdateError> {
        self.refresh_latest().await?;
        let result = self.build_check_result(current_version);
        self.lock().last_check = Some(result.clone());
        Ok(result)
    }

    /// 最近一次「检查更新」的结果；本进程还没检查过时返回 `checked:false`。
    ///
    /// 供 `/api/update/status`（前端 60 秒轮询）与定时任务共用同一份缓存。
    pub fn last_check(&self) -> Value {
        match self.lock().last_check.clone() {
            Some(result) => result,
            None => json!({ "checked": false }),
        }
    }

    /// 拉取最新 Release（对应 refreshLatest）。
    /// 404 表示仓库还没有发布任何版本，按「无更新」处理（latest = null）。
    async fn refresh_latest(&self) -> Result<(), UpdateError> {
        let repository = self.repository();
        let url = format!("{GITHUB_API}/repos/{repository}/releases/latest");
        let response = client::fetch_with_egress(&url, &client::github_headers(), REQUEST_TIMEOUT_MS)
            .await?;

        let status = response.status().as_u16();
        if status == 404 {
            self.lock().latest = None;
            return Ok(());
        }
        if status == 403 || status == 429 {
            // 文案照抄 Node（含环境变量提示）：403 多为匿名请求限额用尽
            return Err(UpdateError::new(
                "GitHub 接口访问受限（可能是请求频率超限）。稍后再试，或设置 WORKBUDDY_GITHUB_TOKEN 提高限额",
            ));
        }
        if !response.status().is_success() {
            return Err(UpdateError::new(format!("GitHub 返回 HTTP {status}")));
        }

        let payload: Value = response
            .json()
            .await
            .map_err(|error| UpdateError::new(format!("解析 GitHub 响应失败: {error}")))?;
        let text = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        // tag 剥掉可选的 `v` 前缀（Node: `String(payload.tag_name || '').replace(/^v/i, '')`）
        let raw_tag = text("tag_name");
        let tag = raw_tag
            .strip_prefix('v')
            .or_else(|| raw_tag.strip_prefix('V'))
            .unwrap_or(&raw_tag)
            .to_string();
        let name = {
            let name = text("name");
            if name.is_empty() { raw_tag.clone() } else { name }
        };
        // notes 截断到 4000 字符（HTML 正文字段，直接给界面展示）
        let notes: String = text("body").chars().take(4000).collect();
        let page_url = {
            let url = text("html_url");
            if url.is_empty() {
                format!("https://github.com/{repository}/releases")
            } else {
                url
            }
        };
        let latest = json!({
            "tag": tag,
            "name": name,
            "notes": notes,
            "publishedAt": text("published_at"),
            "pageUrl": page_url,
            "prerelease": payload.get("prerelease").and_then(Value::as_bool) == Some(true),
            "asset": pick_installer(payload.get("assets")),
        });
        self.lock().latest = Some(latest);
        Ok(())
    }

    /// 组装 check 的响应（对应 buildCheckResult，字段逐个对齐）
    fn build_check_result(&self, current_version: &str) -> Value {
        let version = current_version.trim().to_string();
        let (latest, repository, download_dir, task) = {
            let guard = self.lock();
            (
                guard.latest.clone(),
                guard.repository.clone(),
                guard.download_dir.clone(),
                guard.task.clone(),
            )
        };
        let latest_tag = latest
            .as_ref()
            .and_then(|value| value.get("tag"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let comparison = if latest.is_some() && !version.is_empty() {
            compare_versions(latest_tag, &version)
        } else {
            None
        };
        // 只有带 url 的资产才算「可下载」
        let asset = latest
            .as_ref()
            .and_then(|value| value.get("asset"))
            .filter(|value| {
                value.get("url").and_then(Value::as_str).map(|url| !url.is_empty()).unwrap_or(false)
            })
            .cloned();
        let latest_text = |key: &str| -> Value {
            match latest.as_ref().and_then(|value| value.get(key)) {
                Some(Value::String(text)) if !text.is_empty() => Value::String(text.clone()),
                Some(Value::Bool(flag)) => Value::Bool(*flag),
                _ => Value::Null,
            }
        };
        json!({
            "currentVersion": if version.is_empty() { Value::Null } else { Value::String(version) },
            "latestVersion": if latest_tag.is_empty() { Value::Null } else { Value::String(latest_tag.to_string()) },
            // 无法解析版本号时不谎报「有更新」，界面按 null 显示为「无法比较」
            "hasUpdate": match comparison {
                Some(order) => Value::Bool(order > 0),
                None => Value::Null,
            },
            "comparison": match comparison {
                Some(order) if order > 0 => Value::String("newer".to_string()),
                Some(order) if order < 0 => Value::String("older".to_string()),
                Some(_) => Value::String("same".to_string()),
                None => Value::Null,
            },
            "releaseName": latest_text("name"),
            "notes": latest_text("notes"),
            "publishedAt": latest_text("publishedAt"),
            "pageUrl": latest_text("pageUrl"),
            "prerelease": latest
                .as_ref()
                .and_then(|value| value.get("prerelease"))
                .and_then(Value::as_bool)
                == Some(true),
            "asset": asset.clone().unwrap_or(Value::Null),
            "repository": repository,
            "downloadSupported": asset.is_some(),
            // 安装包形态（"nsis" / "dmg"）：界面据此决定文案 ——
            // Windows 要提示 UAC 提权、macOS 是挂载磁盘映像后手动拖进应用程序
            "installerKind": installer_kind(),
            "downloadDir": download_dir.to_string_lossy(),
            "task": task.map(|task| task.to_json()).unwrap_or(Value::Null),
        })
    }

    // ─── 下载 ───────────────────────────────────────────────

    /// 下载进度（前端轮询）；没有任务时为 null（对应 Node 的 taskSummary）
    pub fn get_progress(&self) -> Value {
        self.lock()
            .task
            .clone()
            .map(|task| task.to_json())
            .unwrap_or(Value::Null)
    }

    /// 取消进行中的下载并清理半截文件（对应 cancelDownload）。
    /// 没有进行中的任务时返回 `{canceled:false}`。
    pub fn cancel_download(&self) -> Value {
        let (cancelable, flag) = {
            let mut guard = self.lock();
            let Some(task) = guard.task.as_mut() else {
                return json!({ "canceled": false });
            };
            if !task.active {
                return json!({ "canceled": false });
            }
            task.canceled = true;
            (true, guard.cancel_flag.clone())
        };
        if cancelable {
            if let Some(flag) = flag {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        json!({ "canceled": true })
    }

    /// 启动安装包下载（对应 startDownload）。
    ///
    /// 同一时刻只跑一个：已有进行中的任务时**直接返回它的快照**，
    /// 避免用户连点导致多个 200MB 的安装包同时落盘。
    /// 后台跑真正的下载（接口立刻返回，进度由前端轮询）。
    pub fn start_download(&self, url: &str, name: &str) -> Result<Value, UpdateError> {
        // ① 已有进行中的任务 → 直接复用（**在校验 URL 之前**，与 Node 的判定顺序一致）
        if let Some(task) = self.lock().task.clone().filter(|task| task.active) {
            return Ok(task.to_json());
        }

        let target = assert_downloadable(url)?;
        let fallback_name = target
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("")
            .to_string();
        let filename = safe_file_name(if name.is_empty() { &fallback_name } else { name });
        let download_dir = self.download_dir();
        if let Err(error) = std::fs::create_dir_all(&download_dir) {
            return Err(UpdateError::new(format!("创建下载目录失败: {error}")));
        }
        let file_path = download_dir.join(&filename);

        // 同版本重复下载时直接覆盖，不留一堆历史安装包。
        // 删除失败（文件被占用）不在这里报错 —— 让后面的写入自己报，
        // 与 Node 的 `try { rmSync } catch { /* 让写入自己报错 */ }` 一致
        let _ = std::fs::remove_file(&file_path);

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let snapshot = {
            let mut guard = self.lock();
            let task = Task {
                active: true,
                done: false,
                canceled: false,
                error: None,
                filename: filename.clone(),
                path: Some(file_path.to_string_lossy().to_string()),
                received: 0,
                total: 0,
            };
            guard.task = Some(task.clone());
            guard.cancel_flag = Some(cancel_flag.clone());
            task.to_json()
        };

        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            service.run_download(target, file_path, cancel_flag).await;
        });
        Ok(snapshot)
    }

    /// 真正的下载（对应 runDownload）。
    ///
    /// 与 Node 相同的收尾约定：无论成功、失败还是取消，都清掉半截文件
    /// （留着会被误当成可运行的安装包）、把 path 置 null、active 置 false；
    /// 取消是**正常路径**（记「下载已取消」日志，不写 error）。
    async fn run_download(
        &self,
        target: url::Url,
        file_path: PathBuf,
        cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let outcome = self.download_stream(&target, &file_path, &cancel_flag).await;
        let canceled = cancel_flag.load(std::sync::atomic::Ordering::SeqCst);
        let succeeded = matches!(&outcome, Ok(_)) && !canceled;

        // 清理半截文件：**只有失败与取消才删**（Node 的 catch 分支才 rmSync）。
        // 成功路径绝不能删 —— 那会把刚下好的安装包一起带走。
        if !succeeded {
            let _ = std::fs::remove_file(&file_path);
        }

        {
            let mut guard = self.lock();
            let Some(task) = guard.task.as_mut() else {
                return;
            };
            task.active = false;
            // 按引用匹配：下面还要用 outcome 打日志，不能让这里把它 move 走
            match &outcome {
                Ok(size) if !canceled => {
                    task.done = true;
                    task.received = *size;
                    task.total = *size;
                    task.path = Some(file_path.to_string_lossy().to_string());
                    task.error = None;
                }
                // 取消与失败：path 置 null（留着的半成品会被误当成可运行的安装包）
                Ok(_) => {
                    task.path = None;
                    task.error = None;
                }
                Err(error) => {
                    task.path = None;
                    if canceled {
                        // 取消导致的读取中断不算错误（Node 同）
                        task.error = None;
                    } else {
                        task.error = Some(error.message.clone());
                    }
                }
            }
        }
        // 日志与控制台输出放在锁外（避免持锁做 IO）
        if canceled {
            logging::log("[Update]", "下载已取消");
            return;
        }
        match outcome {
            Ok(size) => logging::log(
                "[Update]",
                &format!(
                    "✅ 安装包已就绪: {}（{:.1} MB）",
                    file_path.display(),
                    size as f64 / 1024.0 / 1024.0
                ),
            ),
            Err(error) => logging::log("[Update]", &format!("❌ {}", error.message)),
        }
    }

    /// 流式下载主体：返回落盘字节数。`Err` 是失败原因（取消由调用方读标志区分）。
    ///
    /// ── 为什么用 `tokio::select!` 而不是只在 chunk 之间查标志 ────
    /// Node 版把 AbortController 的 signal 挂在整个 fetch 上，取消会**立刻**
    /// 中断「建连 / 等响应头」阶段。若这里只在 `while let Some(chunk)` 里查标志，
    /// 那么卡在等响应头时（GitHub 慢、连接挂着）取消要等到第一块数据才生效 ——
    /// 实测就是「cancel 返回 200，但进度长时间停在 active:true」。
    /// select! 让「等到下一块数据」与「等到取消请求」赛跑，取消即时生效。
    async fn download_stream(
        &self,
        target: &url::Url,
        file_path: &PathBuf,
        cancel_flag: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<u64, UpdateError> {
        let headers = vec![
            ("User-Agent".to_string(), USER_AGENT.to_string()),
            ("Accept".to_string(), "application/octet-stream".to_string()),
        ];
        // 建连 + 等响应头阶段也要能被取消（reason 见函数注释）
        let response = tokio::select! {
            result = client::fetch_with_egress(target.as_str(), &headers, REQUEST_TIMEOUT_MS) => {
                result?
            }
            _ = wait_cancel(cancel_flag) => return Ok(0),
        };
        if !response.status().is_success() {
            return Err(UpdateError::new(format!(
                "下载失败：GitHub 返回 HTTP {}",
                response.status().as_u16()
            )));
        }
        let declared = response.content_length().unwrap_or(0);
        if declared > MAX_INSTALLER_BYTES {
            return Err(UpdateError::new(format!(
                "安装包体积异常（{} MB），已中止",
                declared / 1024 / 1024
            )));
        }
        {
            let mut guard = self.lock();
            if let Some(task) = guard.task.as_mut() {
                task.total = declared;
            }
        }
        logging::log(
            "[Update]",
            &format!(
                "开始下载 {}{}",
                file_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default(),
                if declared > 0 {
                    format!("（{:.1} MB）", declared as f64 / 1024.0 / 1024.0)
                } else {
                    String::new()
                }
            ),
        );

        // 落盘：先写临时后缀再原子改名？Node 是**直接写目标文件** ——
        // 保持一致：失败/取消路径都会把文件删掉，不会留下可被误执行的文件。
        let mut file = tokio::fs::File::create(file_path)
            .await
            .map_err(|error| UpdateError::new(format!("创建下载文件失败: {error}")))?;
        let mut stream = response.bytes_stream();
        let mut received: u64 = 0;
        loop {
            // 「等下一块数据」与「等到取消」赛跑：body 数据块之间的间隔可能很长
            // （GitHub 限速、连接僵住），只在 chunk 到达后查标志会让取消失效
            let chunk = tokio::select! {
                item = stream.next() => item,
                _ = wait_cancel(cancel_flag) => {
                    // 取消：返回 Ok(已收字节数)，由调用方按 canceled 标志走正常收尾
                    return Ok(received);
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.map_err(|error| {
                if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    // 主动取消会让 reqwest 报「body 读取中断」，这不是失败
                    UpdateError::new("下载已取消")
                } else {
                    UpdateError::new(format!(
                        "下载中断: {}",
                        crate::server::core::egress::describe_error_detail(&error)
                    ))
                }
            })?;
            received += chunk.len() as u64;
            if received > MAX_INSTALLER_BYTES {
                return Err(UpdateError::new("安装包超过体积上限，已中止"));
            }
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|error| UpdateError::new(format!("写入安装包失败: {error}")))?;
            // 进度在锁内更新（只加两个数字），随即放锁继续读流
            let mut guard = self.lock();
            if let Some(task) = guard.task.as_mut() {
                task.received = received;
                // 未声明 content-length 时用已收字节估算总量，保证进度条不是死的
                if task.total == 0 {
                    task.total = received;
                }
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|error| UpdateError::new(format!("写入安装包失败: {error}")))?;
        drop(file);

        if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(received);
        }
        // 磁盘写入完成后再核一次大小，避免「进度 100% 但文件不完整」
        let size = std::fs::metadata(file_path).map(|meta| meta.len()).unwrap_or(0);
        if declared > 0 && size != declared {
            return Err(UpdateError::new(format!(
                "安装包不完整（期望 {declared} 字节，实际 {size} 字节）"
            )));
        }
        Ok(size)
    }
}

/// 等到「取消」被请求（轮询标志，20ms 一次）。
///
/// 为什么不用 `tokio::sync::Notify` / oneshot：取消标志的写入方是
/// `cancel_download`（持锁的同步函数），它已经有原子标志可读；再引入一条
/// 通知通道就要在锁里多维护一个可选发送端，还要处理「发送端正好被替换」
/// 的竞态。20ms 轮询的代价是每 20ms 一次原子读，可以忽略，换来的是
/// 与 Node 的 AbortController 同等量级的响应速度（用户点「取消」到
/// 实际中断，肉眼无差别）。
async fn wait_cancel(flag: &Arc<std::sync::atomic::AtomicBool>) {
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ─── 进程级句柄 ─────────────────────────────────────────────

/// 进程级更新管理器（与 config / logging 同一模式）：
/// bootstrap 时装入一次，路由层与服务启动路径共用同一实例。
static GLOBAL: std::sync::OnceLock<UpdateManager> = std::sync::OnceLock::new();

/// 初始化进程级管理器（幂等）
pub fn init_global(manager: UpdateManager) -> UpdateManager {
    let _ = GLOBAL.set(manager.clone());
    manager
}
