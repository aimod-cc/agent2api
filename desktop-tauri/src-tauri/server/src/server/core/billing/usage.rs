//! 积分 / 额度查询（对照 Node 版 workbuddy-billing.mjs 的 queryUsage 一族）。
//!
//!   个人账号：POST /v2/billing/meter/get-user-resource   → getPersonalUsage
//!   企业账号：POST /v2/billing/meter/get-enterprise-user-usage → getEnterpriseUsage
//!   组合简报：queryCreditsSummary（只给三个数：总剩余 / 套餐基础 / 平台奖励）
//!   用量提示：POST /v2/billing/meter/get-dosage-notify
//!
//! 从 billing/mod.rs 拆出（单文件行数约定）。**所有请求都经 `call_billing`**
//! （定义在 mod.rs），本文件只负责把上游的嵌套结构翻译成前端要的形状。

use serde_json::{json, Map, Value};

use crate::server::core::endpoints::RESPONSE_CODE_OK;
use crate::server::core::account_store::state::json_number;

use super::commodity::{self, codes, is_bonus, is_daily_credit, is_plan_base, label_of, plan_priority};
use super::request::{
    js_int_string, js_truthy, number_or_zero, parse_time, time_or_null, to_int,
    timestamp_json, CallOptions, BILLING_DOSAGE_NOTIFY, BILLING_ENTERPRISE_USAGE,
    BILLING_USER_RESOURCE,
};
use super::{BillingError, BillingService};

impl BillingService {
    // ─── 积分查询 ───────────────────────────────────────────

    /// 查询账号积分/额度：企业账号走企业接口，个人账号走资源接口。
    pub async fn query_usage(
        &self,
        session: Option<&Value>,
        locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        let is_enterprise = active
            .get("account")
            .and_then(|account| account.get("enterpriseId"))
            .map(|value| match value {
                Value::String(text) => !text.is_empty(),
                Value::Null => false,
                other => js_truthy(other),
            })
            .unwrap_or(false);
        if is_enterprise {
            self.get_enterprise_usage(&active).await
        } else {
            self.get_personal_usage(&active, locale).await
        }
    }

    /// 个人账号：POST /v2/billing/meter/get-user-resource
    pub async fn get_personal_usage(
        &self,
        session: &Value,
        locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let result = self
            .call_billing(
                BILLING_USER_RESOURCE,
                CallOptions { session: Some(session), locale, ..Default::default() },
            )
            .await?;
        // 结果路径：data.Response.Data.Accounts[]
        let resources: Vec<Value> = result
            .data
            .get("Response")
            .and_then(|value| value.get("Data"))
            .and_then(|value| value.get("Accounts"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut plan_resources: Vec<Value> = resources
            .iter()
            .map(|item| {
                let package_code = item
                    .get("PackageCode")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let is_daily = is_daily_credit(&package_code);
                // Node: `Number(r.CycleCapacitySizePrecise) || 0` —— 非数字/0 都是 0
                let total = number_or_zero(item.get("CycleCapacitySizePrecise"));
                let left = number_or_zero(item.get("CycleCapacityRemainPrecise"));
                // 下面几处都是 Node 的 `parseTime(x) || ...` 形态：parseTime 解析不出
                // 返回 **0**，而 0 在 JS 里是假值 —— 所以「解出 0」与「解不出」等价，
                // `time_or_null` 就是这条语义（0 / 缺失 → None → JSON null）。
                let start_at = time_or_null(item.get("DeductionStartTime"))
                    .or_else(|| time_or_null(item.get("CycleStartTime")));
                let expire_at = time_or_null(if is_daily {
                    item.get("CycleEndTime")
                } else {
                    item.get("DeductionEndTime")
                });
                // Node: `isDaily ? null : (parseTime(r.CycleEndTime) || 0) + 1000 || null`
                // `(0) + 1000 = 1000` 是**真值**，所以非日额度包拿不到 CycleEndTime 时
                // refreshAt 会是 1000 这个哨兵值 —— 照抄（前端据此判断「没有刷新时间」
                // 的行为也照抄）
                let refresh_at = if is_daily {
                    None
                } else {
                    Some(parse_time(item.get("CycleEndTime")) + 1000)
                };
                // expireAt 的排序键：缺失视为 Infinity（排在最后）
                let expire_sort = expire_at.map(|value| value as f64).unwrap_or(f64::INFINITY);
                let name = label_of(&package_code)
                    .map(str::to_string)
                    .or_else(|| {
                        item.get("PackageName")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| package_code.clone());
                // 逐键组装而不是 json! 字面量：Node 里 `id: r.ResourceId` 在字段
                // 缺失时是 undefined，`JSON.stringify` 会**整个丢掉这个键**；
                // json! 的 null 会把它留在响应里，前端按键存在与否判空就会分叉
                let mut resource = Map::new();
                if let Some(resource_id) = item.get("ResourceId") {
                    if !resource_id.is_null() {
                        resource.insert("id".to_string(), resource_id.clone());
                    }
                }
                resource.insert("packageCode".to_string(), Value::String(package_code));
                resource.insert("name".to_string(), Value::String(name));
                resource.insert("isDaily".to_string(), Value::Bool(is_daily));
                // 额度一律写成「整数形态的数」（json_number）：serde_json 把
                // f64 的 100.0 输出成 `100.0`，而 Node 的 JSON.stringify(100)
                // 是 `100` —— 让面板上的「1500 / 1500」与 Node 版逐字一致
                resource.insert("total".to_string(), json_number(total));
                resource.insert("used".to_string(), json_number((total - left).max(0.0)));
                resource.insert("left".to_string(), json_number(left));
                resource.insert("startAt".to_string(), timestamp_json(start_at));
                resource.insert("expireAt".to_string(), timestamp_json(expire_at));
                resource.insert("refreshAt".to_string(), timestamp_json(refresh_at));
                // 排序辅助字段：排序后会被删掉，不属于契约
                resource.insert("__expireSort".to_string(), json_number(expire_sort));
                resource.insert(
                    "autoRenew".to_string(),
                    Value::Bool(number_or_zero(item.get("AutoRenewFlag")) == 1.0),
                );
                Value::Object(resource)
            })
            .collect();

        // 与桌面端一致：先按套餐优先级，同优先级再按到期时间升序
        plan_resources.sort_by(|a, b| {
            let code_a = a.get("packageCode").and_then(Value::as_str);
            let code_b = b.get("packageCode").and_then(Value::as_str);
            let diff = plan_priority(code_a) - plan_priority(code_b);
            if diff != 0 {
                return diff.cmp(&0);
            }
            let sort_a = a.get("__expireSort").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
            let sort_b = b.get("__expireSort").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
            sort_a.partial_cmp(&sort_b).unwrap_or(std::cmp::Ordering::Equal)
        });
        // 去掉内部排序辅助字段（它不属于契约）
        for item in plan_resources.iter_mut() {
            if let Some(object) = item.as_object_mut() {
                object.remove("__expireSort");
            }
        }

        let mut usage_total = 0.0;
        let mut usage_left = 0.0;
        let mut usage_used = 0.0;
        for item in &plan_resources {
            usage_total += item.get("total").and_then(Value::as_f64).unwrap_or(0.0);
            usage_left += item.get("left").and_then(Value::as_f64).unwrap_or(0.0);
            usage_used += item.get("used").and_then(Value::as_f64).unwrap_or(0.0);
        }

        // 当前生效套餐按等级挑选（旗舰 > 进阶 > 专业 > 青春 > 试用），
        // 而不是简单取列表首个（日额度包金额小但到期最晚，会误判为 free）
        let find_code = |wanted: &[&str]| -> Option<&Value> {
            plan_resources.iter().find(|item| {
                item.get("packageCode")
                    .and_then(Value::as_str)
                    .map(|code| wanted.contains(&code))
                    .unwrap_or(false)
            })
        };
        let flagship = find_code(&[codes::FLAGSHIP]);
        let advanced = find_code(&[codes::ADVANCED]);
        let pro = find_code(&[codes::PRO_YEAR, codes::PRO_MON, codes::PRO_MON_PLUS]);
        let youth = find_code(&[codes::YOUTH]);
        let trial = find_code(&[
            codes::PRO_TRIAL_MON,
            codes::PRO_TRIAL_YEAR,
            codes::FREE_MON_INTL,
            codes::GIFT,
            codes::FREE_MON,
        ]);
        let active = flagship
            .or(advanced)
            .or(pro)
            .or(youth)
            .or(trial)
            .or_else(|| plan_resources.first());

        let edition_type = if flagship.is_some() {
            "flagship"
        } else if advanced.is_some() {
            "advanced"
        } else if youth.is_some() && pro.is_none() {
            "youth"
        } else if pro.is_some() {
            "pro"
        } else if trial.is_some() {
            "pro_trial"
        } else {
            "free"
        };

        Ok(json!({
            "kind": "personal",
            "editionType": edition_type,
            "isPro": pro.is_some(),
            "isTrial": trial.is_some() && pro.is_none() && advanced.is_none()
                && flagship.is_none() && youth.is_none(),
            "planName": active
                .and_then(|item| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            "packageCode": active
                .and_then(|item| item.get("packageCode"))
                .cloned()
                .unwrap_or(Value::Null),
            // 三个合计是**字符串**（Node 用 String(...)）：前端的 `esc()` 直接渲染，
            // 数字形态会让「0」与「0.0」这种差异冒到界面上
            "usageTotal": js_int_string(usage_total),
            "usageLeft": js_int_string(usage_left),
            "usageUsed": js_int_string(usage_used),
            // 这四个都是 `active?.xxx || null` / `active?.xxx ?? false`：active 为
            // null 时也给出 null / false（键始终存在，不是 undefined）
            "expireAt": active
                .and_then(|item| item.get("expireAt"))
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(Value::Null),
            "refreshAt": active
                .and_then(|item| item.get("refreshAt"))
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(Value::Null),
            "autoRenew": active
                .and_then(|item| item.get("autoRenew"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "resources": plan_resources,
            "raw": result.data,
        }))
    }

    /// 企业账号：POST /v2/billing/meter/get-enterprise-user-usage
    pub async fn get_enterprise_usage(&self, session: &Value) -> Result<Value, BillingError> {
        let result = self
            .call_billing(
                BILLING_ENTERPRISE_USAGE,
                CallOptions { session: Some(session), ..Default::default() },
            )
            .await?;
        // Node: `result.raw ?? {}` → `data?.data || data || result.data || responseData`
        let response_data = result.raw.clone().unwrap_or_else(|| json!({}));
        let usage_data = response_data
            .get("data")
            .and_then(|value| value.get("data"))
            .or_else(|| response_data.get("data"))
            .or_else(|| {
                if result.data.is_null() { None } else { Some(&result.data) }
            })
            .cloned()
            .unwrap_or_else(|| response_data.clone());

        let account_type = session
            .get("account")
            .and_then(|account| account.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let edition_type = if account_type == "ultimate" {
            "ultimate"
        } else if account_type == "exclusive" {
            "exclusive"
        } else {
            "enterprise"
        };

        let limit_num = usage_data.get("limitNum").and_then(Value::as_f64);
        if let Some(limit_num) = limit_num {
            let credit = usage_data
                .get("credit")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            // Node: `usageData.cycleResetTime ? parseTime(usageData.cycleResetTime) : null`
            // 注意：判断的是**原始值**的真值，然后才 parseTime（可能得 0）——
            // 所以有值但解析失败时给的是 0，不是 null
            let refresh_at = match usage_data.get("cycleResetTime") {
                Some(value) if js_truthy(value) => {
                    Value::from(parse_time(Some(value)))
                }
                _ => Value::Null,
            };
            if limit_num == -1.0 {
                // limitNum === -1 表示不限量
                return Ok(json!({
                    "kind": "enterprise",
                    "editionType": edition_type,
                    "unlimited": true,
                    "usageLeft": commodity::UNLIMITED_USAGE_SENTINEL,
                    "usageTotal": commodity::UNLIMITED_USAGE_SENTINEL,
                    "usageUsed": js_int_string(credit),
                    "refreshAt": refresh_at,
                    "resources": [],
                    "raw": usage_data,
                }));
            }
            return Ok(json!({
                "kind": "enterprise",
                "editionType": edition_type,
                "unlimited": false,
                "usageLeft": js_int_string(limit_num - credit),
                "usageTotal": js_int_string(limit_num),
                "usageUsed": js_int_string(credit),
                "refreshAt": refresh_at,
                "resources": [],
                "raw": usage_data,
            }));
        }
        Ok(json!({
            "kind": "enterprise",
            "editionType": edition_type,
            "unlimited": false,
            "usageLeft": "0",
            "usageTotal": "0",
            "usageUsed": "0",
            "resources": [],
            "raw": if usage_data.is_null() { Value::Null } else { usage_data },
        }))
    }

    /// 轻量余额查询：只取剩余额度，供限额判定等热路径使用。
    ///
    /// 不限量返回 `Some(None)`（即 Infinity 的等价物），
    /// 解析不出剩余量时返回 Err（对应 Node 抛 WorkBuddyBillingError）。
    // Node 版 workbuddy-billing.mjs 同名导出的对等物（Node 里只被 smoke test 覆盖）：
    // 保留为「按账号查余额」的轻量入口，与 query_usage 的完整简报互为补充。
    #[allow(dead_code)]
    pub async fn fetch_available_credits(
        &self,
        session: Option<&Value>,
    ) -> Result<Option<i64>, BillingError> {
        let usage = self.query_usage(session, None).await?;
        if usage.get("unlimited").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(None);
        }
        match usage.get("usageLeft").and_then(to_int) {
            Some(left) => Ok(Some(left)),
            None => Err(BillingError::new("积分响应缺少可解析的剩余量", 502)),
        }
    }

    // ─── 积分简报 ───────────────────────────────────────────

    /// 积分简报：只返回三个数 —— 总剩余积分 / 套餐基础积分 / 平台奖励积分。
    /// 总剩余含加油包等全部资源包，因此可能大于后两项之和（买了加油包时）。
    /// 企业账号没有套餐/奖励之分：totalLeft 即剩余额度，后两项为 null
    /// （不限量时为 null + unlimited 标记）。
    pub async fn query_credits_summary(
        &self,
        session: Option<&Value>,
        locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let usage = self.query_usage(session, locale).await?;
        let kind = usage.get("kind").and_then(Value::as_str).unwrap_or("personal");
        if kind == "enterprise" {
            let unlimited = usage.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
            return Ok(json!({
                "kind": "enterprise",
                "unlimited": unlimited,
                "totalLeft": if unlimited {
                    Value::Null
                } else {
                    usage.get("usageLeft").and_then(to_int).map(Value::from).unwrap_or(Value::Null)
                },
                "planLeft": Value::Null,
                "bonusLeft": Value::Null,
            }));
        }
        let mut total_left: i64 = 0;
        let mut plan_left: i64 = 0;
        let mut bonus_left: i64 = 0;
        if let Some(resources) = usage.get("resources").and_then(Value::as_array) {
            for item in resources {
                let left = item.get("left").and_then(to_int).unwrap_or(0);
                total_left += left;
                let code = item.get("packageCode").and_then(Value::as_str);
                if is_plan_base(code) {
                    plan_left += left;
                } else if is_bonus(code) {
                    bonus_left += left;
                }
            }
        }
        Ok(json!({
            "kind": "personal",
            "unlimited": false,
            "totalLeft": total_left,
            "planLeft": plan_left,
            "bonusLeft": bonus_left,
        }))
    }

    /// 用量提示（productFeatures.BillingNotice 开启时桌面端会轮询）。
    /// Node 版 workbuddy-billing.mjs 同名导出 `getDosageNotify` 的对等物：
    /// 桌面端前端目前未接入该轮询，保留以便按开关启用时不改后端。
    #[allow(dead_code)]
    pub async fn get_dosage_notify(&self, session: Option<&Value>) -> Result<Value, BillingError> {
        let result = self
            .call_billing(
                BILLING_DOSAGE_NOTIFY,
                CallOptions { session, expect_code_ok: false, ..Default::default() },
            )
            .await?;
        if result.code == Some(RESPONSE_CODE_OK) {
            Ok(result.data)
        } else {
            Ok(Value::Null)
        }
    }
}
