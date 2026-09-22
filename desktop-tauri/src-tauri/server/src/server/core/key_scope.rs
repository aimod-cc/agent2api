//! 「这次请求命中的是哪把网关 Key」以及它带来的**可用提供商 / 可用模型**限制
//! （R9，参考 OmniProxy 的 `allowed_provider_ids` / `allowed_models`）。
//!
//! ── 为什么单独一个模块（而不是塞进 api_keys.rs）────────────────
//! 两个理由：
//!   1. `api_keys.rs` 是**配置的存取层**（config.json 的 `apiKeys` 字段：解析、
//!      增删改、白名单判定），它不认识 axum；本模块是**请求作用域**的东西
//!      （塞进请求扩展、被 handler 与转发层读），它要认识 axum 的 `Request`。
//!      混在一起会让那个文件同时背两种关注点；
//!   2. 限制的**传播路径**（中间件放入 → handler 取出 → 传进转发入参）横跨
//!      `http.rs` / `api/*` / `core::upstream` 三处，落在一个有名字的类型上
//!      比散落成「某个字段的约定」好找。
//!
//! ── 中间件怎么把「命中的那把 Key」传下去（本项目选的方式）──────
//! 用 **axum 的请求扩展**（`http::Request::extensions_mut().insert(...)`）：
//!   - 类型安全、无字符串键，拼错名字是编译错误；扩展里取不到就是 `None`，
//!     与本需求「没命中 Key = 不限制」的语义天然对齐（不需要区分「没有」与
//!     「有但不限制」—— 两者行为完全相同）；
//!   - **不碰请求体与请求头**：本项目多处按「客户端原始 body 的 sha256」做
//!     去重键（`api::chat` / `api::protocol`），往 body 里塞东西会改变去重键；
//!     往请求头里塞则会污染转发时透传给上游的头集合（适配器会读
//!     `client_headers`，见 `ProviderAdapter::build_chat_request`）。
//!   - 与既有做法一致：axum 的 `Extension<T>` 是「请求作用域的附加数据」的
//!     标准落点，本项目 `ServerState` 走 `State` 是因为它全局唯一，而「命中的
//!     Key」是**逐请求**的，正是 Extension 的用途。
//!
//! ── 没有命中 Key 的三种情形（都必须 = 不限制）────────────────
//!   1. **免鉴权模式**：一把启用的 Key 都没有 → 中间件直接放行，本类型根本不
//!      会被放进扩展。请求照常转发（这与改造前逐字一致）。
//!   2. **环境变量 Key**（`WORKBUDDY_PROXY_API_KEY`）：它是启动注入的通用口令，
//!      没有列表记录可挂白名单 → `api_keys::entry_for_key` 返回 None，
//!      按不限制处理。
//!   3. Key 在两次读盘之间被删/停用：同样取不到记录。
//! 三种都收敛成「扩展里没有 `KeyScope`」这一个判据，于是所有消费点只写一次
//! `scope.map_or(true, |s| ...)`。

use std::collections::HashSet;

use axum::extract::Request;

use crate::server::core::api_keys::ApiKeyEntry;

/// 本次请求命中的 Key 所带来的限制（**已归一**：空列表 = 不限制）。
///
/// 存 `HashSet`（小写形式）而不是原始 `Vec<String>`：判定发生在**每一次选路
/// 尝试**与**每一个模型列表条目**上，逐条 `eq_ignore_ascii_case` 扫描是 O(n)
/// 次字符串比较；归一成小写集合后是一次哈希查找。大小写不敏感与整条链路同口径
/// （模型名、provider id 的比对历来忽略大小写，见 `model_rules` 模块头）。
#[derive(Clone, Debug, Default)]
pub struct KeyScope {
    /// 允许的 provider id（小写），空集合 = 不限制
    allowed_providers: HashSet<String>,
    /// 允许的对外模型名（小写），空集合 = 不限制
    allowed_models: HashSet<String>,
}

impl KeyScope {
    /// 由一条 Key 记录构造（`allowed*` 为空 = 该维度不限制）
    pub fn from_entry(entry: &ApiKeyEntry) -> Self {
        Self {
            allowed_providers: normalize(&entry.allowed_providers),
            allowed_models: normalize(&entry.allowed_models),
        }
    }

    /// 是否限制提供商（界面上「有没有勾」的判据，也用于日志措辞）
    pub fn restricts_providers(&self) -> bool {
        !self.allowed_providers.is_empty()
    }

    /// 是否限制模型
    pub fn restricts_models(&self) -> bool {
        !self.allowed_models.is_empty()
    }

    /// 允许请求 `model`（**对外模型名**，含映射 alias）？
    ///
    /// 比对忽略大小写与首尾空白：模型名会从 HTTP 请求体里原样取出来，
    /// 客户端多一个空格不该被当成「不在白名单里」。
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty() || self.allowed_models.contains(&normalize_one(model))
    }

    /// 允许路由到 `provider`（provider id）？
    pub fn allows_provider(&self, provider: &str) -> bool {
        self.allowed_providers.is_empty() || self.allowed_providers.contains(&normalize_one(provider))
    }

    /// 白名单的可读摘要（日志与错误文案用；空 = `不限制`）
    pub fn describe(&self) -> String {
        let providers = if self.restricts_providers() {
            sorted(&self.allowed_providers).join("、")
        } else {
            "不限制".to_string()
        };
        let models = if self.restricts_models() {
            format!("{} 个模型", self.allowed_models.len())
        } else {
            "不限制".to_string()
        };
        format!("提供商 {providers}；模型 {models}")
    }
}

/// 一组白名单归一成小写集合（去空白、去空项）
fn normalize(list: &[String]) -> HashSet<String> {
    list.iter()
        .map(|item| normalize_one(item))
        .filter(|item| !item.is_empty())
        .collect()
}

/// 单个名字归一：去首尾空白 + 转小写
fn normalize_one(value: &str) -> String {
    value.trim().to_lowercase()
}

/// 集合排序后的副本（日志文案要稳定，HashSet 的迭代顺序每次都不同）
fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = set.iter().cloned().collect();
    out.sort();
    out
}

/// 把命中的 Key 记录放进请求扩展（**只有鉴权中间件调它**）。
///
/// 不覆盖已有值：中间件只跑一次，但将来若有人再挂一层鉴权中间件，
/// 「第一个命中的 Key 说了算」比「最后写的覆盖前面的」更容易解释。
pub fn attach(request: &mut Request, scope: KeyScope) {
    if request.extensions().get::<KeyScope>().is_none() {
        request.extensions_mut().insert(scope);
    }
}

/// handler 侧取用：`Option<KeyScope>` 的便捷判定（`None` = 不限制）。
///
/// 单独给函数而不是让各处写 `scope.map_or(true, |s| s.allows_model(m))`：
/// 「取不到 = 放行」这条语义只该有一处实现 —— 将来若改成「取不到也拒绝」，
/// 只改这里。
pub fn allows_model(scope: Option<&KeyScope>, model: &str) -> bool {
    scope.map_or(true, |scope| scope.allows_model(model))
}

/// 同上，提供商维度
pub fn allows_provider(scope: Option<&KeyScope>, provider: &str) -> bool {
    scope.map_or(true, |scope| scope.allows_provider(provider))
}
