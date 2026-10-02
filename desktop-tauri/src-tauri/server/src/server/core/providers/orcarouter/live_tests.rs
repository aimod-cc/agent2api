//! OrcaRouter 的 **live 检查**：用真实 Key 打真实上游，走本次实现的 provider 代码路径。
//!
//! ── 为什么它与其它测试分开 ──────────────────────────────────
//! 这里会真发计费请求，因此**只在环境变量 `ORCAROUTER_API_KEY` 存在时运行**；
//! 没有它时打印一行说明并跳过（本仓库既有的「需要外部凭据」测试都是这个口径，
//! 不引入会拖垮普通 `cargo test` 的强制依赖）。独立验证器带那把 Key 跑这一条。
//!
//! ── 它证明什么 ──────────────────────────────────────────────
//!   1. **账号落库 → 目录发现**：`save_manual_credentials` 落的账号，经
//!      [`list_models_for`] 能拉到这台工作区**真实可用**的模型清单（带 Key，
//!      而不是匿名全量）；
//!   2. **能力过滤正确**：文本下拉里每一条都必须声明本网关能说的端点类型；
//!      多模态下拉里每一条都必须显式声明 image 输入；
//!   3. **推理真通**：用 [`OrcaRouterAdapter::build_chat_request`] 造出的请求
//!      （同一个适配器、同一条凭据读取链）真发一次 `chat/completions`，
//!      拿到 200 与内容 —— 这是「provider 接线通了」的最终证据。
//!
//! ── 凭据纪律 ────────────────────────────────────────────────
//! Key 只从环境变量读，只进本进程的出网请求；**不进日志、不进断言消息、
//! 不进 `assert_eq!` 的左右值**（失败信息会被 CI 打出来）。所有断言里的
//! Key 引用一律用「非空」这类间接判据。

use serde_json::{json, Value};

use super::*;
use crate::server::core::providers::adapter::ProviderAdapter;

/// 环境变量名（独立验证器按这个名字注入那把 Key）。
const ENV_KEY: &str = "ORCAROUTER_API_KEY";

/// 读到 Key 就返回它，否则打印跳过说明并返回 `None`。
fn live_key() -> Option<String> {
    match std::env::var(ENV_KEY) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => {
            eprintln!("[live] 未设置 {ENV_KEY}，跳过 OrcaRouter live 检查");
            None
        }
    }
}

/// 清掉可能被别的用例留下的 origin 覆盖（本文件要打**官方**上游）。
fn use_official_origins() {
    std::env::remove_var("ORCA_AUTH_BASE_URL");
    std::env::remove_var("ORCA_API_BASE_URL");
    std::env::remove_var("ORCA_BASE_URL");
}

fn ids_of(payload: &Value) -> Vec<String> {
    payload
        .get("models")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 目录发现 + 能力过滤（真实 Key → 真实 `GET /v1/models`）。
#[tokio::test]
async fn live_catalog_through_the_provider_lists_only_compatible_models() {
    let Some(key) = live_key() else { return };
    let _env = super::tests::env_guard();
    use_official_origins();

    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-live-catalog");
    let store = AccountStore::with_db(Some(db));
    let account = save_manual_credentials(&store, &json!({ "apiKey": key }), Some("live 检查"))
        .expect("账号要能落库");
    assert_eq!(account.get("provider").and_then(Value::as_str), Some("orcarouter"));

    let text = list_models_for(&store, "", "text", None).await;
    assert_eq!(text["source"], "live", "带 Key 时目录必须以 live 为权威");
    assert_eq!(text["catalogSource"], "https://api.orcarouter.ai/v1/models");
    assert_eq!(text["degraded"], false);
    let text_ids = ids_of(&text);
    assert!(!text_ids.is_empty(), "这台工作区的文本目录不该为空");
    // 每一条都必须是「本网关说得了话」的 chat 模型（能力过滤在服务端生效）
    assert!(text_ids.iter().all(|id| id.contains('/')), "模型 id 保留 vendor/model 命名空间");
    let not_text: Vec<&Value> = text["models"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    let declared = item
                        .get("supportedEndpointTypes")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter().filter_map(Value::as_str).any(|value| {
                                super::catalog::SUPPORTED_ENDPOINT_TYPES.contains(&value)
                            })
                        })
                        .unwrap_or(false);
                    !declared
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(not_text.is_empty(), "文本下拉里出现了未声明可用端点类型的模型");
    // 响应里没有凭据
    assert!(!text.to_string().contains(&key));

    // 多模态（图片理解）：每一条都必须显式声明 image 输入 —— fail closed
    let vision = list_models_for(&store, "", "multimodal", Some("image")).await;
    assert_eq!(vision["source"], "live");
    let vision_ids = ids_of(&vision);
    for id in &vision_ids {
        assert!(text_ids.contains(id), "{id} 不在文本目录里，却进了多模态下拉");
    }
    let without_image: Vec<&Value> = vision["models"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    !item
                        .get("inputModalities")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .any(|value| value.eq_ignore_ascii_case("image"))
                        })
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        without_image.is_empty(),
        "多模态下拉里出现了未声明 image 输入的模型（fail closed 被破坏）"
    );
    eprintln!(
        "[live] 文本目录 {} 条，其中声明 image 输入 {} 条",
        text_ids.len(),
        vision_ids.len()
    );
}

/// 推理真通：用本次实现的适配器造请求，真发一次 `chat/completions`。
#[tokio::test]
async fn live_chat_request_through_the_provider_succeeds() {
    let Some(key) = live_key() else { return };
    let _env = super::tests::env_guard();
    use_official_origins();

    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-live-chat");
    let store = AccountStore::with_db(Some(db));
    save_manual_credentials(&store, &json!({ "apiKey": key }), Some("live 检查"))
        .expect("账号要能落库");

    let text_catalog = list_models_for(&store, "", "text", None).await;
    // ── 挑一个这台工作区**确实可用**的模型 ──────────────────────
    // 目录给的是「这把 Key 看得见」的集合，但工作区可能对其中一部分做了
    // 模型级封禁（实测 `openai/gpt-5.5` 对本 Key 回 403 `model_access_denied` /
    // `block_key_scope`）。因此这里按目录顺序**逐个试到第一个 200**，
    // 并把实际成功的那个模型名打出来 —— 这正是「provider 接线通了」的证据，
    // 而不是把一次模型级拒绝当成链路故障。
    let catalog_ids = ids_of(&text_catalog);
    assert!(!catalog_ids.is_empty(), "目录里至少要有一个文本模型");
    // 优先试一把**已实测可用**的模型（本 Key 2026-10-02 实测通过），再按目录
    // 顺序继续扫 —— 工作区对模型级封禁是不透明的（目录给了名字不代表这把 Key
    // 能用），因此这里只能逐个试，不能假设第一条就能用。
    let mut candidates: Vec<String> = vec!["deepseek/deepseek-v4-pro".to_string()];
    for id in catalog_ids.iter() {
        if !candidates.contains(id) {
            candidates.push(id.clone());
        }
    }

    // 用适配器自己的凭据读取链取 Key（与转发路径同一条）
    let accounts = store.accounts_for_provider("orcarouter");
    let account_id = accounts
        .first()
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let token = ORCAROUTER_ADAPTER
        .ensure_access_token(&store, &account_id)
        .await
        .expect("取 Key");
    assert!(!token.trim().is_empty());

    let mut last_status = 0u16;
    let mut last_detail = String::new();
    let mut success: Option<(String, String)> = None;
    for model in candidates.iter().take(24) {
        let plan = ORCAROUTER_ADAPTER
            .build_chat_request(
                &json!({ "auth": { "accessToken": token } }),
                &json!({
                    "model": model,
                    "messages": [{ "role": "user", "content": "Reply with the single word: pong" }],
                    "max_tokens": 16,
                    "stream": false,
                }),
                &axum::http::HeaderMap::new(),
            )
            .unwrap_or_else(|error| panic!("构造请求失败：{}", error.message));
        assert_eq!(plan.url, "https://api.orcarouter.ai/v1/chat/completions");
        assert!(
            !plan.url.contains("www.orcarouter.ai"),
            "推理绝不指向认证 origin"
        );

        // 真发（同一个出网客户端工厂，与适配器的转发路径一致）
        let mut request = crate::server::core::egress::client_for(None)
            .post(&plan.url)
            .timeout(std::time::Duration::from_secs(60));
        for (name, value) in &plan.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.json(&plan.body).send().await.unwrap_or_else(|error| {
            panic!(
                "推理请求失败：{}",
                crate::server::core::egress::describe_error_detail(&error)
            )
        });
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status == 200 {
            let content = body
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            assert!(!content.trim().is_empty(), "推理响应里没有内容（模型 {model}）");
            success = Some((model.clone(), content));
            break;
        }
        last_status = status;
        // 错误分类走**适配器自己的**判据（同一份代码路径）
        let class = ORCAROUTER_ADAPTER.classify_error(status, &body);
        last_detail = format!("{model} → {class:?}");
        // 401 是终态（凭据被吊销），继续换模型没有意义
        assert_ne!(status, 401, "凭据被上游拒绝（401），换模型也救不回来：{last_detail}");
    }

    let (model, content) = success.unwrap_or_else(|| {
        panic!("试了目录里的前几个模型都没有成功；最后一次：HTTP {last_status} {last_detail}")
    });
    eprintln!(
        "[live] 推理成功（模型 {model}，响应内容长度 {} 字符）",
        content.chars().count()
    );

}
