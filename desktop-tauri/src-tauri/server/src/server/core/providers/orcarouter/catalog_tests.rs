//! 目录解析 / 能力过滤 / 兜底种子的验收测试。
//!
//! ── 为什么这组测试单独一个文件 ────────────────────────────────
//! `catalog.rs` 本身已经八百多行，把它自己的测试再堆进去会让「实现」与
//! 「验收」在同一屏里互相淹没。这里用 `#[path]` 挂进 `catalog` 模块（见
//! `catalog.rs` 末尾的 `mod tests` 声明），因此可以访问私有项。
//!
//! ── 这组测试盯住的三件事 ────────────────────────────────────
//!   1. **能力过滤是 fail closed 的**：拿不到目录证据就不放行（未声明
//!      `architecture.input_modalities` 的模型绝不进多模态下拉）；
//!   2. **fixture 覆盖六类目录条目**：纯文本 chat / 带图 chat / embedding /
//!      图片生成 / 视频 / 重排 —— 前三类要能选出来，后三类要能被自己的
//!      能力挑出来，且都不混进文本下拉；
//!   3. **种子只在 live 失败时使用**：live 成功的结果里不掺任何种子条目，
//!      而种子的元数据（尤其 GPT-5.5 的思考档位）必须逐字保留。

use serde_json::json;

use super::*;

/// 一份**目录 fixture**：六类条目各一条，形状逐字取自
/// `GET https://api.orcarouter.ai/v1/models` 的实测响应（2026-10-02）。
fn fixture() -> Vec<Value> {
    vec![
        json!({
            "id": "vendor/text-only",
            "name": "Vendor: Text Only",
            "owned_by": "vendor",
            "context_length": 128000,
            "max_completion_tokens": 8192,
            "architecture": { "input_modalities": ["text"] },
            "supported_endpoint_types": ["openai", "openai-response"],
        }),
        json!({
            "id": "vendor/vision",
            "name": "Vendor: Vision",
            "owned_by": "vendor",
            "context_length": 200000,
            "architecture": { "input_modalities": ["text", "image", "file"] },
            "supported_endpoint_types": ["openai", "anthropic"],
        }),
        json!({
            "id": "vendor/embed",
            "name": "Vendor: Embed",
            "owned_by": "vendor",
            "architecture": { "input_modalities": ["text"] },
            "supported_endpoint_types": ["embeddings"],
        }),
        json!({
            "id": "vendor/draw",
            "name": "Vendor: Draw",
            "owned_by": "vendor",
            "architecture": { "input_modalities": ["text"] },
            "supported_endpoint_types": ["image-generation"],
        }),
        json!({
            "id": "vendor/movie",
            "name": "Vendor: Movie",
            "owned_by": "vendor",
            "architecture": { "input_modalities": ["text"] },
            "supported_endpoint_types": ["openai-video"],
        }),
        json!({
            "id": "vendor/rank",
            "name": "Vendor: Rank",
            "owned_by": "vendor",
            "architecture": { "input_modalities": ["text"] },
            "supported_endpoint_types": ["jina-rerank"],
        }),
        // 没有任何能力声明的条目：**任何**下拉都不该出现它（fail closed）
        json!({ "id": "vendor/opaque", "name": "Vendor: Opaque" }),
    ]
}

fn ids(items: &[Value]) -> Vec<String> {
    items.iter().map(item_id).collect()
}

fn select(items: &[Value], kind: ModelKind, modality: Option<Modality>) -> Vec<String> {
    ids(&items
        .iter()
        .filter(|item| match (kind, modality) {
            (ModelKind::Multimodal(_), Some(modality)) => {
                kind.accepts(item) && declares_input_modality(item, modality.as_str())
            }
            _ => kind.accepts(item),
        })
        .cloned()
        .collect::<Vec<Value>>())
}

#[test]
fn text_chat_filter_keeps_only_declarable_chat_models() {
    let items = normalize_items(&fixture());
    let text = select(&items, ModelKind::Text, None);
    assert_eq!(text, vec!["vendor/text-only", "vendor/vision"]);
    // 图片生成 / 视频 / 重排 / embedding 都不得混进文本下拉
    for absent in ["vendor/draw", "vendor/movie", "vendor/rank", "vendor/embed", "vendor/opaque"] {
        assert!(!text.iter().any(|id| id == absent), "{absent} 不得进文本下拉");
    }
}

#[test]
fn multimodal_filter_is_fail_closed_and_requires_declared_image_input() {
    let items = normalize_items(&fixture());
    let vision = select(&items, ModelKind::Multimodal(Modality::Image), Some(Modality::Image));
    assert_eq!(vision, vec!["vendor/vision"], "只有显式声明 image 输入的 chat 模型才进图片理解下拉");
    // 未声明模态的文本模型不得因「名字看起来能看图」被放行
    assert!(!vision.iter().any(|id| id == "vendor/text-only"));
    assert!(!vision.iter().any(|id| id == "vendor/opaque"));

    // 目录里没有模型声明 audio / video 输入 → 对应下拉必须为空（fail closed）
    assert!(select(&items, ModelKind::Multimodal(Modality::Audio), Some(Modality::Audio)).is_empty());
    assert!(select(&items, ModelKind::Multimodal(Modality::Video), Some(Modality::Video)).is_empty());
}

#[test]
fn each_other_capability_selects_only_its_own_endpoint_type() {
    let items = normalize_items(&fixture());
    assert_eq!(select(&items, ModelKind::Embedding, None), vec!["vendor/embed"]);
    assert_eq!(select(&items, ModelKind::Image, None), vec!["vendor/draw"]);
    assert_eq!(select(&items, ModelKind::Video, None), vec!["vendor/movie"]);
    assert_eq!(select(&items, ModelKind::Rerank, None), vec!["vendor/rank"]);
    // 每种能力用的 `?capability=` 参数（服务端裁剪用）
    assert_eq!(ModelKind::Text.capability_param(), Some("chat"));
    assert_eq!(ModelKind::Embedding.capability_param(), Some("embedding"));
    assert_eq!(ModelKind::Image.capability_param(), Some("image"));
    assert_eq!(ModelKind::Video.capability_param(), Some("video"));
    assert_eq!(ModelKind::Rerank.capability_param(), Some("rerank"));
    assert_eq!(
        ModelKind::Multimodal(Modality::Image).capability_param(),
        Some("chat"),
        "多模态理解先满足 chat，再按模态二次过滤"
    );
}

#[test]
fn parse_rejects_unknown_kinds_and_multimodal_without_modality() {
    assert_eq!(ModelKind::parse("text", None), Some(ModelKind::Text));
    assert_eq!(ModelKind::parse("", None), Some(ModelKind::Text));
    assert_eq!(ModelKind::parse("embedding", None), Some(ModelKind::Embedding));
    assert_eq!(
        ModelKind::parse("multimodal", Some(Modality::Image)),
        Some(ModelKind::Multimodal(Modality::Image))
    );
    // 说了多模态却没给模态 → None（调用方必须按「不认识的用途」拒绝，
    // 而不是悄悄退回文本口径：那会把图片模型混进对话下拉）
    assert_eq!(ModelKind::parse("multimodal", None), None);
    assert_eq!(ModelKind::parse("nonsense", None), None);
    assert_eq!(Modality::parse("vision"), Some(Modality::Image));
    assert_eq!(Modality::parse("IMAGE"), Some(Modality::Image));
    assert_eq!(Modality::parse("pdf"), None);
}

#[test]
fn namespace_is_preserved_verbatim_and_metadata_is_mapped() {
    let items = normalize_items(&[json!({
        "id": "deepseek/deepseek-v4-pro",
        "name": "DeepSeek: DeepSeek V4 Pro",
        "context_length": 1048576,
        "max_completion_tokens": 384000,
        "architecture": { "input_modalities": ["text"] },
        "supported_endpoint_types": ["openai"],
    })]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "deepseek/deepseek-v4-pro", "vendor/model 命名空间逐字保留");
    assert_eq!(items[0]["maxInputTokens"], 1048576);
    assert_eq!(items[0]["maxOutputTokens"], 384000);
    assert_eq!(items[0]["supportsImages"], false);
    assert_eq!(items[0]["input_modalities"][0], "text");
    // 未知的 owned_by 由 id 的命名空间兜底
    assert_eq!(items[0]["owned_by"], "deepseek");
}

#[test]
fn normalization_rejects_broken_entries_and_deduplicates() {
    let items = normalize_items(&[
        json!({ "id": "" }),
        json!({ "name": "没有 id" }),
        json!({ "id": "vendor/dup" }),
        json!({ "id": "VENDOR/DUP" }),
        json!({ "id": "x".repeat(MAX_ID_CHARS + 1) }),
        json!({ "id": 42 }),
        json!({ "id": "vendor/ok" }),
    ]);
    assert_eq!(ids(&items), vec!["vendor/dup", "vendor/ok"], "空 / 超长 / 非字符串 id 一律丢弃，大小写去重");
}

#[test]
fn seed_metadata_is_verified_and_live_results_never_mix_seed_in() {
    let seed = seed_catalog();
    let seed_ids = ids(&seed);
    for expected in [
        "openai/gpt-5.5",
        "anthropic/claude-opus-4.8",
        "google/gemini-3.5-flash",
        "deepseek/deepseek-v4-pro",
        "orcarouter/auto",
    ] {
        assert!(seed_ids.iter().any(|id| id == expected), "种子必须含 {expected}");
    }
    let gpt = seed.iter().find(|item| item_id(item) == "openai/gpt-5.5").expect("gpt-5.5");
    assert_eq!(gpt["maxOutputTokens"], 128000);
    assert_eq!(gpt["supportsImages"], true, "实测 input_modalities 含 image");
    assert_eq!(gpt["supportsReasoning"], true);
    // orcarouter/auto 实测未声明上下文 → 缺键（而不是 0）
    let auto = seed.iter().find(|item| item_id(item) == "orcarouter/auto").expect("auto");
    assert!(auto.get("maxInputTokens").is_none(), "未声明的元数据不得编造");

    // 思考档位：GPT-5.5 的四档必须逐字保留
    assert_eq!(reasoning_efforts("openai/gpt-5.5"), &["low", "medium", "high", "xhigh"]);
    assert!(reasoning_efforts("openai/gpt-5.5").contains(&"xhigh"));
    assert_eq!(reasoning_efforts("google/gemini-3.5-flash"), &[] as &[&str]);
    assert!(!reasoning_supported("vendor/text-only"), "不认识的族不得声明思考能力");

    // **live 结果里不掺种子**：一份只带一条真实模型的 live 清单归一后就是一条
    let live = normalize_items(&[json!({
        "id": "vendor/live-only",
        "architecture": { "input_modalities": ["text"] },
        "supported_endpoint_types": ["openai"],
    })]);
    assert_eq!(ids(&live), vec!["vendor/live-only"]);
    assert!(!ids(&live).iter().any(|id| seed_ids.contains(id)));
}

#[test]
fn text_chat_requires_a_supported_endpoint_type() {
    // 只说 anthropic / gemini 的模型也算「本网关说得了话」（转发走 OpenAI 形态，
    // 上游按模型自己翻译）；但只说非文本端点类型的不算
    assert!(is_text_chat(&json!({ "supported_endpoint_types": ["gemini"] })));
    assert!(is_text_chat(&json!({ "supported_endpoint_types": ["ANTHROPIC"] })));
    assert!(!is_text_chat(&json!({ "supported_endpoint_types": ["image-generation"] })));
    assert!(!is_text_chat(&json!({})));
    assert!(!is_text_chat(&json!({ "supported_endpoint_types": [] })));
}
