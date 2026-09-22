//! CatPaw 消息指纹与指纹链（Agent2API 二期 W3p-T-d1；移植来源
//! `catpaw-upstream-messages.mjs` 的 `messageFingerprint` 与
//! `catpaw-upstream-client.mjs` 第 380-395 行的增量定位逻辑）。
//!
//! ── 指纹是干什么用的 ────────────────────────────────────────
//! CatPaw 上游是**有状态**协议：同一客户端会话（`x-session-id`）复用同一个
//! conversationId，后续轮次的 round 只提交「增量消息」（见 UPSTREAM_PROTOCOL §5）。
//! 代理要回答的问题是：**客户端这次提交的历史里，哪一段上游已经见过了？**
//! 办法是给每条归一化后的消息算一个稳定指纹，把「已同步过的指纹」按序存在
//! 会话注册表里；新请求到达时用最后一条已同步指纹在客户端历史里定位，
//! 之后的部分就是增量。
//!
//! ── 为什么必须是 SHA-256 且**逐字对齐**原实现 ────────────────
//! 指纹只在本进程内比对（注册表是内存表），换算法不会「读不懂旧数据」；
//! 但**同一个客户端的一串请求**会横跨一次进程内升级到下一次请求：
//! 若算法变了，会话注册表在进程重启后本来就是空的 —— 真正的影响是
//! 「同一次运行内前后不一致」不可能发生，而**跨版本**重启后所有长会话
//! 都要重建（上一进程存下的 conversationId 只在进程内，重启后本来也失效）。
//! 结论：真正要守的是「**归一化输出 → 指纹** 这一步的确定性」——
//! 同一份归一化消息任何时候都必须得到同一个指纹，否则同一会话在
//! 两轮之间会误判「客户端改写了历史」而被作废重建（全量 round）。
//! 因此这里的序列化方式严格照抄原实现（见 [`message_fingerprint`] 的说明），
//! 不做任何「顺手优化」（比如顺手把 reasoningContent 也塞进去）。
//!
//! ── 指纹里有什么 / 没有什么（原实现的口径，别改）────────────
//! 参与：消息的 `type`（或 `role`）、每个块的 `type` / `text` / `toolCallId` /
//! `toolName` / `toolParams` / `toolResult`。
//! **不参与**：`messageId`（归一化时对缺失 id 的消息会生成随机 UUID，
//! 塞进去等于每条消息都不稳定 —— 反过来说，正是因为它不参与，归一化那边
//! 才可以放心用随机 UUID 兜底，见 `messages::message_id`）、`finished`、
//! `reasoningContent`、图片块的 `imageUrl`（图片块只贡献字面量 `"image_url"`）。
//! 空 text 块（`text` 与 `reasoningContent` 都为空）整体跳过 —— 上游返回的
//! 纯工具调用消息常带一个空 text 块，而客户端回显时会把它丢掉
//! （content 为 null），保留会导致同一条消息两边指纹不同、误判历史被改写。

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// JavaScript 真值语义（`x || y`、`if (x)` 的判定）。
///
/// 归一化与指纹两处都要用 `a || b` 这类「按真值取第一个」的判定
/// （原实现是 JS），而 `serde_json` 里没有 undefined，`null` 也不等价于
/// 「缺失」—— 两处各写一份判断迟早会走岔，所以只定义这一份。
/// 放在本文件的原因：指纹是全模块最依赖「JS 口径」的地方，
/// 归一路径上的判定（`message.tool_calls || message.toolCalls`、
/// `if (reasoning)`、`block.toolName &&`）与它是同一套语义。
///
/// 注意与「空值合并」（`??`）的区别：`??` 只跳过 null/undefined，
/// 而 `""`、`0`、`false` 都能通过 —— 那些地方在调用点用 `is_null()` 表达。
pub(super) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        // serde_json 的数字不可能是 NaN，as_f64 失败只可能是内部异常
        // （真出现时按「非零」处理，与 JS 对非零数字的真值判定一致）
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 单条**归一化后**消息的指纹（64 位十六进制小写；对应
/// `createHash('sha256').update(JSON.stringify(parts)).digest('hex')`）。
///
/// ── 序列化方式（必须逐字对齐的部分）─────────────────────────
/// 原实现不是对消息对象做 `JSON.stringify`（那会把字段顺序、messageId
/// 都卷进来），而是先摊平成一个**字符串数组** `parts`，再对数组做
/// `JSON.stringify` 后哈希 —— 即先拼出 `["user","text","你好"]` 这样的
/// JSON 文本，再对这个文本取 SHA-256（UTF-8 字节）。
/// `parts` 的组装顺序（第一条固定是消息类型，其余按块顺序）：
///   1. `message.type || message.role || ''`
///   2. 每个块：跳过「text 且 text 与 reasoningContent 都为空」的块；
///      然后依次 push 块的 `type`（缺失/假值 → 空串）、`text`（**存在即 push**，
///      哪怕是空串或 null）、`toolCallId`（真值才 push）、`toolName`（真值才 push）、
///      `toolParams`（存在即 push）、`toolResult`（存在即 push，先转字符串）。
///
/// `serde_json::to_string(Vec<Value>)` 与 JS `JSON.stringify(array)` 在
/// 字符串转义上一致（`"` / `\` / 控制字符转义，非 ASCII 原样输出），
/// 因此逐字节等价。
pub fn message_fingerprint(message: &Value) -> String {
    let mut parts: Vec<Value> = Vec::with_capacity(8);
    // `message.type || message.role || ''`
    let head = message
        .get("type")
        .filter(|value| js_truthy(value))
        .or_else(|| message.get("role").filter(|value| js_truthy(value)))
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    parts.push(head);

    // 非数组 content（含缺失、字符串、null）按空处理：原实现的 for-of 在
    // 非可迭代值上会抛异常，而指纹计算只在归一化产物上发生（content 必为数组），
    // 这里选「不抛」是为了让本函数保持无副作用、无错误路径的纯函数。
    let blocks: &[Value] = message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for block in blocks {
        let Some(object) = block.as_object() else {
            // 非对象块：JS 里 `block?.type` 是 undefined、`block.text` 也是
            // undefined，最终只往 parts 里贡献一个空串
            parts.push(Value::String(String::new()));
            continue;
        };
        if is_blank_text_block(object) {
            continue;
        }
        match object.get("type") {
            Some(value) if js_truthy(value) => parts.push(value.clone()),
            _ => parts.push(Value::String(String::new())),
        }
        if let Some(text) = object.get("text") {
            parts.push(text.clone());
        }
        if let Some(call_id) = object.get("toolCallId").filter(|value| js_truthy(value)) {
            parts.push(call_id.clone());
        }
        if let Some(name) = object.get("toolName").filter(|value| js_truthy(value)) {
            parts.push(name.clone());
        }
        if let Some(params) = object.get("toolParams") {
            parts.push(params.clone());
        }
        if let Some(result) = object.get("toolResult") {
            // `String(block.toolResult)`：归一化保证这里是字符串（恒等），
            // 其它类型只在手写上游消息时可能出现，按 JSON 字面量渲染
            parts.push(Value::String(stringify_lossy(result)));
        }
    }

    // 序列化失败（Value 不可能失败）时用空串兜底：宁可得到一个恒定指纹，
    // 也不 panic（release 是 panic=abort，见架构文档 §8）
    let serialized = serde_json::to_string(&parts).unwrap_or_default();
    sha256_hex(&serialized)
}

/// 一串归一化消息的指纹链（原实现 `prepared.messages.map(messageFingerprint)`）。
pub fn fingerprints_for(messages: &[Value]) -> Vec<String> {
    messages.iter().map(message_fingerprint).collect()
}

/// 指纹链的定位结果（`locate_increment` 的返回值）。
///
/// 对应原实现 `catpaw-upstream-client.mjs` 的 `syncIndex`：
/// `Incremental { start }` = `syncIndex + 1`（`start == messages.len()` 表示
/// 「本次没有新东西」，调用方按原实现回落成只提交最后一条消息）；
/// `Mismatch` = 客户端历史里找不到最后一条已同步指纹 → 作废旧 conversation、
/// 新建并**全量** round（原实现的 `sync-mismatch` 失效路径）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChainPosition {
    /// 增量从 `messages[start..]` 开始（`start` 可能等于 `messages.len()`）
    Incremental {
        /// 增量起点（归一化消息数组的下标）
        start: usize,
    },
    /// 指纹对不上：客户端压缩/改写了历史，需重建会话全量提交
    Mismatch,
}

/// 在客户端本次的归一化消息里定位增量起点。
///
/// ── 算法（照抄原实现，不要「加强」成整链比对）────────────────
/// 只取注册表存的**最后一条**已同步指纹，在本次消息列表里做 `lastIndexOf`
/// （即从后往前找第一条命中）。原实现就是这么做的：注册表的 `synced`
/// 是「已提交给上游的指纹」序列，其末元素就是「上游最后见过的那条消息」，
/// 客户端历史里它的位置之后即为增量。
/// 之所以不做「逐条前缀比对」：客户端可能在同一会话里替换掉更早的
/// 消息（压缩），只要最后一条能对上，中间的任何差异都会被
/// 「整段增量重新提交」覆盖掉 —— 逐条比对反而会把这种可续接的会话判成失效。
pub fn locate_increment(messages: &[Value], synced: &[String]) -> ChainPosition {
    // 原实现是 `const lastSynced = synced.at(-1); ... lastSynced ? ... : -1` ——
    // 末尾元素为**空串**时按「没有已同步指纹」处理（JS 真值判定）。
    // 正常来源（`message_fingerprint` 的产物）恒为 64 位十六进制，不会为空；
    // 这里对齐判定只是为了不把一个手工构造的空串当成「找不到 → 会话重建」。
    let Some(last_synced) = synced.last().filter(|value| !value.is_empty()) else {
        // 注册表里没有指纹链（全新映射，或末尾是空串）：全部消息都是增量
        // （原实现 `syncIndex = -1` → `slice(0)`）。
        return ChainPosition::Incremental { start: 0 };
    };
    let fingerprints = fingerprints_for(messages);
    match fingerprints.iter().rposition(|fingerprint| fingerprint == last_synced) {
        Some(index) => ChainPosition::Incremental { start: index + 1 },
        None => ChainPosition::Mismatch,
    }
}

/// 空 text 块判定：`block.type === 'text' && !block.text && !block.reasoningContent`。
///
/// 两个字段都缺失 / 都是假值（空串、0、false、null）时才跳过；
/// 只要 `reasoningContent` 非空就不跳过（此时 parts 里会多一个空串 text，
/// 与原实现一致 —— 原实现**从不**把 reasoningContent 本身放进指纹）。
fn is_blank_text_block(object: &Map<String, Value>) -> bool {
    if object.get("type").and_then(Value::as_str) != Some("text") {
        return false;
    }
    let text_blank = object.get("text").map(|value| !js_truthy(value)).unwrap_or(true);
    let reasoning_blank = object
        .get("reasoningContent")
        .map(|value| !js_truthy(value))
        .unwrap_or(true);
    text_blank && reasoning_blank
}

/// JS `String(value)` 的近似：字符串取原值，标量按 JS 的字面量渲染，
/// 对象/数组退化成 JSON 文本。
///
/// 归一化产物里 `toolResult` 恒为字符串（`toolResultContent` 的返回类型
/// 只有 `string`），所以本函数实际只会走第一个分支。其余分支只为
/// 「手写上游消息」这种非正规输入兜底：JS 对对象会给出 `[object Object]`、
/// 对数组会 `join(',')`，这里统一渲染成 JSON —— 差异不可达（同一进程内
/// 收发都用本函数，指纹一致性不受影响），记录下来是为了让读代码的人
/// 不必再推一遍。
fn stringify_lossy(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// SHA-256 → 64 位十六进制小写（对应 `digest('hex')`）。
///
/// 与 `api::chat` 的 `sha256_hex` 是同一件事，但那个函数是私有的、
/// 且接收 `&[u8]`（请求体去重键）；这里是对已序列化的字符串取哈希，
/// 语义不同（那条链路不能因为本模块的存在而改动），因此各自保留。
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
