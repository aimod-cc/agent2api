//! 远程默认词库：从仓库拉取 `sensitive-words.json`，按版本把新词合并进用户词表。
//!
//! ── 为什么要有这一层（替代「升级客户端才更新词表」）────────────
//! 在此之前，默认词表的更新路径只有一条：改 `engine.rs` 的 `DEFAULT_TERMS`
//! 与 `DEFAULT_TERM_MIGRATIONS`，发一个新版本，用户升级后启动时才合并一次。
//! 上游的拦截规则是随时在变的（同一个字段名今天放过、明天就拦），而客户端
//! 升级是**低频且用户可控**的动作 —— 把词表绑在版本号上，等于「上游变了，
//! 用户得等下一个 Release 且愿意升级才能恢复」。本模块把词表从二进制里解耦
//! 出来：仓库根目录的 `sensitive-words.json` 是**可随时追加**的那一份，
//! 客户端启动时拉一次、之后由定时任务每 10 分钟拉一次。
//!
//! ── 与内置默认词表的关系（两条路都要留着）───────────────────
//!   - 内置那份（`engine.rs` 的 `DEFAULT_TERMS`）是**离线兜底**：拉不到网络、
//!     仓库文件被删、JSON 被改坏，词表都还是可用的。它同时是全新用户的初始词表。
//!   - 远程那份是**增量**：只往里补词，从不删除、不覆盖用户自己改过的词表。
//!     两者的版本号共用一个命名空间（`defaultsVersion`），远程版本高于本地
//!     已合并版本时才合并 —— 于是「内置已含 v4、远程还是 v3」不会把版本号
//!     往回退。
//!
//! ── 合并规则：只补不删（与 `migrate_defaults` 同一口径）──────
//! 远程词表按「远端 version 高于本地已合并版本」触发，把远端词条里**本地还没有
//! 的**（忽略大小写比对）追加到末尾。用户主动删掉的词**会被补回来** —— 这是
//! 与内置迁移表有意的差异：内置迁移表按「版本区间」登记词条，用户删了就不再
//! 补（保护删除权）；远程词表是**当前全量**，无法区分「用户删了」与「新增的」。
//! 取舍的理由：远程那份的定位是「上游拦截规则的同步」，漏一个词的代价是
//! 整条请求 400（用户看到的是「代理坏了」），比「用户删过的词又回来了」重得多。
//! 用户要彻底禁掉某个词，正确做法是关掉该 provider 的脱敏、或删掉词后不要再
//! 让远端同步（把「敏感词库更新」任务关掉）。
//!
//! ── 出网与失败处理 ──────────────────────────────────────────
//! 复用 `core::update::client::fetch_with_egress`（直连优先、Clash 兜底），
//! 因此「GitHub 需要代理才能访问」的环境与软件版本检查走同一条路。
//! **任何失败都只记 verbose 日志**（网络抖动是常态，10 分钟一次的任务不该
//! 每轮都往日志页写一条 error），返回值交给调用方展示摘要。

use std::sync::OnceLock;

use serde_json::Value;

use crate::server::core::update::fetch_with_egress;
use crate::server::logging;

/// 远程词库的 URL（仓库根目录的 `sensitive-words.json`）。
///
/// 用 `raw.githubusercontent.com` 而不是 GitHub API：raw 域名**不计入 API 的
/// 匿名限额**（60 次/小时/IP），而本任务 10 分钟一轮就是 144 次/天 ——
/// 走 API 会直接把限额打满，连累软件版本检查。raw 的响应体就是文件原文，
/// 也不需要解析 base64 的 `content` 字段。
///
/// 环境变量 `AGENT2API_SENSITIVE_WORDS_URL` 可覆盖（fork 的用户指向自己的仓库，
/// 或本地调试时指向 file:// 之外的自建服务）。空串当未设置。
pub const DEFAULT_REMOTE_URL: &str =
    "https://raw.githubusercontent.com/aimod-cc/agent2api/main/sensitive-words.json";

/// 环境变量名（覆盖 [`DEFAULT_REMOTE_URL`]）
const ENV_URL: &str = "AGENT2API_SENSITIVE_WORDS_URL";

/// 拉取超时：与软件版本检查同档（30s）。文件只有几 KB，30s 足够覆盖
/// 「直连失败 → 换 Clash 出口」的两轮尝试。
const FETCH_TIMEOUT_MS: u64 = 30_000;

/// 远程词表条数上限：防止远端文件被改成一份巨大词表把内存与匹配开销撑爆
/// （与 `engine::MAX_TERMS` 同一数量级，这里留一倍余量）。
const MAX_REMOTE_TERMS: usize = 4000;

/// 远程词表：解析后的结果
#[derive(Debug, Clone)]
pub struct RemoteTerms {
    /// 远端声明的版本号
    pub version: u32,
    /// 远端词条（已清洗：trim、去空、去重）
    pub terms: Vec<String>,
}

/// 解析远端 JSON 文本。
///
/// 接受的形态（对 `$comment` 之类说明字段一律忽略，便于词表文件里写注释）：
/// ```json
/// { "version": 4, "terms": ["词1", "词2"] }
/// ```
/// 也接受裸数组 `["词1", "词2"]`（此时版本号按 0 处理 —— 只补词不推进版本，
/// 适合临时手改一份清单做验证）。
///
/// 逐条的宽容口径：
///   - 非对象/非数组 → `Err`（整份不可用，调用方记 verbose 并跳过这一轮）；
///   - `version` 非数字或缺失 → 0（不推进本地版本号，但词条仍会被合并）；
///   - `terms` 里的非字符串项 → 跳过（与 `state.rs` 的 `string_list` 同口径）；
///   - 超长词（> `MAX_TERM_LENGTH`）与空串 → 跳过（由 `normalize_terms` 兜住）。
pub fn parse_remote(text: &str) -> Result<RemoteTerms, String> {
    let parsed: Value = serde_json::from_str(text).map_err(|error| format!("JSON 解析失败：{error}"))?;
    let (version, items) = match &parsed {
        Value::Object(map) => {
            let version = map
                .get("version")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            let items = map
                .get("terms")
                .and_then(Value::as_array)
                .ok_or_else(|| "缺少 terms 数组".to_string())?;
            (version, items)
        }
        Value::Array(items) => (0, items),
        _ => return Err("顶层必须是对象或数组".to_string()),
    };
    let raw: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect();
    // 清洗走与本地词表完全相同的实现（trim / 滤空 / 去重 / 长度上限），
    // 因此「远端加进来的词」与「用户在界面上手填的词」在后续匹配里没有差别
    let mut terms = crate::server::core::desensitize::normalize_terms(&raw);
    if terms.len() > MAX_REMOTE_TERMS {
        terms.truncate(MAX_REMOTE_TERMS);
    }
    Ok(RemoteTerms { version, terms })
}

/// 当前生效的远程词库 URL（环境变量优先）
pub fn remote_url() -> String {
    std::env::var(ENV_URL)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_string())
}

/// 拉取并解析远端词表。
///
/// 返回 `Err` 的两种情形都只是「这一轮不合并」：网络层失败（含换出口后仍失败）
/// 与响应体不可解析。HTTP 非 2xx 也当失败（raw 的 404 是「仓库里没有这个文件」
/// 或「分支名不对」，重试无用但下一轮仍会试 —— 文件是用户自己可以补上的）。
pub async fn fetch() -> Result<RemoteTerms, String> {
    let url = remote_url();
    let headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        // 与软件版本检查同一 UA：便于 GitHub 侧辨认来源
        ("User-Agent".to_string(), "workbuddy-local-proxy".to_string()),
    ];
    let response = fetch_with_egress(&url, &headers, FETCH_TIMEOUT_MS)
        .await
        .map_err(|error| error.message)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取响应失败：{error}"))?;
    parse_remote(&text)
}

/// 进程级「上次同步结果」，供界面/接口查询（不落盘：与定时任务的运行状态同一
/// 取舍 —— 它是本次运行的观察值，重启即空）。
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    /// 上次同步完成的时刻（毫秒，0 = 本次进程还没跑过）
    pub last_sync_at: i64,
    /// 远端版本（成功拉到时的值）
    pub remote_version: u32,
    /// 本次补进来的词条数
    pub added: usize,
    /// 一句话结果（成功 / 失败原因），供界面展示
    pub message: String,
}

static STATE: OnceLock<std::sync::Mutex<SyncState>> = OnceLock::new();

fn state_cell() -> &'static std::sync::Mutex<SyncState> {
    STATE.get_or_init(|| std::sync::Mutex::new(SyncState::default()))
}

/// 读上次同步状态（锁中毒时沿用中毒数据：少一次状态显示远比界面崩掉轻）
pub fn last_state() -> SyncState {
    state_cell()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

fn write_state(next: SyncState) {
    if let Ok(mut guard) = state_cell().lock() {
        *guard = next;
    }
}

/// 同步一次：拉远端 → 版本比对 → 合并进本地词表 → 记录状态。
///
/// 返回给界面/日志的一句话摘要（**不返回 Err**：所有失败都收敛成摘要文案，
/// 与 `scheduled_tasks::run_backend` 的其它任务同一形态 —— 一次拉取失败不该
/// 让整条定时任务链报错）。
///
/// `force` 为 true 时忽略版本号比对，把远端词条全量补一遍（用户点「立即执行」
/// 时用：此时用户的预期是「现在真的去拉一次」，而版本号没变会让「点了没反应」
/// 与「坏掉了」无法区分）。即便如此也**只补不删**，不会破坏用户词表。
pub async fn sync(force: bool) -> String {
    let local = crate::server::core::desensitize::global();
    let before_version = local.defaults_version();
    let before_count = local.term_count();
    let now = logging::now_ms();

    let remote = match fetch().await {
        Ok(remote) => remote,
        Err(error) => {
            // 网络抖动是常态，不写日志库（只 verbose），状态里留痕供界面展示
            logging::verbose("[Desensitize]", &format!("远程词库拉取失败：{error}"));
            write_state(SyncState {
                last_sync_at: now,
                remote_version: 0,
                added: 0,
                message: format!("拉取失败：{error}"),
            });
            return format!("拉取失败：{error}");
        }
    };

    // 版本闸：远端版本不高于本地已合并版本时不做任何事（除非 force）。
    // 注意「远端 version 为 0」的裸数组形态走不到这里 —— 它按 0 处理，
    // 会被这道闸挡下，只有 force 能合并它。
    if !force && (remote.version as f64) <= before_version {
        let message = format!(
            "已是最新（远端 v{}，本地 v{}）",
            remote.version,
            before_version as i64
        );
        write_state(SyncState {
            last_sync_at: now,
            remote_version: remote.version,
            added: 0,
            message: message.clone(),
        });
        return message;
    }

    let added = local.merge_remote_terms(&remote.terms, remote.version as f64);
    let after_version = local.defaults_version();
    let message = if added.is_empty() {
        format!(
            "远端 v{} 无新增词条（当前 {} 个）",
            remote.version,
            local.term_count()
        )
    } else {
        format!(
            "远端 v{} 补入 {} 个词条（{} → {} 个）",
            remote.version,
            added.len(),
            before_count,
            local.term_count()
        )
    };
    logging::log(
        "[Desensitize]",
        &format!("远程词库同步：{message}（本地版本 v{}）", after_version as i64),
    );
    write_state(SyncState {
        last_sync_at: now,
        remote_version: remote.version,
        added: added.len(),
        message: message.clone(),
    });
    message
}
