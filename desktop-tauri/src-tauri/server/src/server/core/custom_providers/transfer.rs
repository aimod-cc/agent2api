//! 自定义提供商**定义**的导入合并（账号导出文件里的 `customProviders` 段）。
//!
//! ── 为什么账号导出要带上提供商定义 ──────────────────────────
//! 自定义账号的 `provider` 字段是「指向自定义提供商的外键」（`custom-…` id）。
//! 只导账号不导定义，换机器后那些账号会指向一个不存在的家：界面挂着、
//! 转发永远失败。因此 v2 起导出文件多一段 `customProviders`（定义不含任何
//! 凭证 —— apiKey 是账号的属性，仍在账号记录里），导入时先合并这一段、
//! 再导账号，外键就不会悬空。
//!
//! ── 合并语义（与账号段同一取向）────────────────────────────
//!   - 按 id 匹配：命中 → 用文件值更新（本机 `createdAt` 保留 —— 排序身份
//!     与优先级同理「取本机值」，文件里的时间轴属于另一台机器）；
//!     未命中 → 追加，`createdAt` 取本机当前时间（同账号 `addedAt` 的口径）。
//!   - **id 绝不重新生成**：账号记录靠它做外键，换了 id 名下账号全部变孤儿。
//!     随机 12 位 hex 的碰撞概率可以忽略，不做同内容不同 id 的去重。
//!   - 文件内同 id 出现多次：先到的赢（与 `validate_model_entries` 的去重取向
//!     一致），后到的记一条警告。
//!
//! 单条坏数据只记一条警告并继续，不毁掉整批；整体写盘失败才算失败
//! （与 `create` / `update` 的落盘纪律一致 —— 新值没有生效就是失败）。
//!
//! 本文件挂在 `custom_providers` 模块下而不是 `account_transfer` 下：
//! 条目形状（八个契约键、models/mappings 的容错归一）是本模块的私有知识，
//! 导入合并复用 `item_of` / `model_entries_of` / `mapping_entries_of`，
//! 放在模块树内就不必把那些函数公开给兄弟模块。

use std::collections::HashSet;

use serde_json::{Map, Value};

// 父模块的私有函数与常量：子模块可见（Rust 隐私规则），但名字仍要显式引入
use super::{item_of, normalize_base_url, read_items, write_items, ID_PREFIX};
use crate::server::logging;

/// 一条定义的处理结果（结构化警告：账号导入端直接并进 `errors` 数组）
pub(crate) struct DefinitionWarning {
    pub id: String,
    pub message: String,
}

/// 合并报告：新增 / 更新了多少家，以及逐条警告
pub(crate) struct MergeReport {
    pub added: usize,
    pub updated: usize,
    pub warnings: Vec<DefinitionWarning>,
}

/// 把导入文件里的自定义提供商定义合并进本机配置并落盘。
///
/// `items` 是导出文件 `customProviders` 键下的原始数组；返回时**已写盘**。
/// 整体失败（写盘不成功）返回 Err，调用方必须当成整批导入失败。
pub(crate) fn merge_imported(items: &[Value]) -> Result<MergeReport, String> {
    let mut current = read_items();
    let mut report = MergeReport { added: 0, updated: 0, warnings: Vec::new() };
    let mut seen: HashSet<String> = HashSet::new();

    for item in items {
        let Some(object) = item.as_object() else {
            report.warnings.push(DefinitionWarning {
                id: String::new(),
                message: "自定义提供商定义必须是 JSON 对象，该条已跳过".to_string(),
            });
            continue;
        };
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        // 前缀防御与 `is_custom_provider_id` 的第一半一致：不带 `custom-` 前缀的
        // id 一律不收（正常导出文件不会有，这条拦的是手改数据）
        if id.is_empty() || !id.starts_with(ID_PREFIX) {
            report.warnings.push(DefinitionWarning {
                id,
                message: "自定义提供商定义缺少合法 id（custom- 前缀），该条已跳过".to_string(),
            });
            continue;
        }
        if !seen.insert(id.clone()) {
            report.warnings.push(DefinitionWarning {
                id,
                message: "导出文件里重复出现的自定义提供商定义，仅采纳第一条".to_string(),
            });
            continue;
        }
        let normalized = match normalize_item(object) {
            Ok(value) => value,
            Err(message) => {
                report.warnings.push(DefinitionWarning { id, message });
                continue;
            }
        };
        match current
            .iter()
            .position(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            Some(index) => {
                // 更新：本机 createdAt 保留（排序身份，见模块头），其余取文件值
                let mut merged = normalized;
                if let Some(local) = current[index].get("createdAt").and_then(Value::as_i64) {
                    if local > 0 {
                        merged["createdAt"] = Value::from(local);
                    }
                }
                current[index] = merged;
                report.updated += 1;
            }
            None => {
                // 新增：createdAt 取本机当前时间（同账号 addedAt 的口径）
                let mut entry = normalized;
                entry["createdAt"] = Value::from(logging::now_ms());
                current.push(entry);
                report.added += 1;
            }
        }
    }

    if !write_items(&current) {
        return Err("保存失败：自定义提供商定义写入未成功（请检查磁盘空间与配置目录权限）".to_string());
    }
    Ok(report)
}

/// 一条导入定义 → 契约形状（复用读侧归一，再补上导入特有的校验）。
///
/// `item_of` 负责补默认键与 models/mappings 的容错归一（坏条目丢弃、超长截断）；
/// 这里只加「门禁」校验：baseUrl 必须是合法 URL（正常流程创建的定义都满足，
/// 报错拦的是手改数据 —— 空基址的定义转发必然失败，收进来只是埋雷）。
/// 展示名缺失不报错：`item_of` 会回落成 id，与读取路径同一取向。
fn normalize_item(object: &Map<String, Value>) -> Result<Value, String> {
    let mut item = item_of(object);
    let base_url = item
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let normalized = normalize_base_url(&base_url)?;
    item["baseUrl"] = Value::String(normalized);
    Ok(item)
}
