//! 运营活动与组合动作（对照 Node 版 workbuddy-billing.mjs 的「活动」与
//! checkinAndReport 部分）。
//!
//!   GET  /v2/activity/workbuddy/banner    运营 banner（需客户端白名单头）
//!   GET  /v2/activity/ambassador/status    大使状态
//!   签到 + 查余额的组合动作（"签到并回报最新积分"）
//!
//! 从 billing/mod.rs 拆出（单文件行数约定）。banner / ambassador **吞掉所有错误
//! 返回 null**（装饰性内容，拉不到不该让首屏报错）；组合动作里的签到失败则收敛成
//! `{success:false}` 交给前端显示成 warn，只有国际版无签到是硬错误。

use serde_json::{json, Map, Value};

use crate::server::core::endpoints::RESPONSE_CODE_OK;
use crate::server::logging;

use super::request::{js_truthy, CallOptions, ACTIVITY_AMBASSADOR, ACTIVITY_BANNER};
use super::{assert_checkin_supported, BillingError, BillingService};

impl BillingService {
    // ─── 活动 ───────────────────────────────────────────────

    /// 运营 banner：需客户端白名单头，否则被拦截（返回 null，与桌面端一致）。
    ///
    /// 这里**吞掉所有错误并返回 null** —— 与 Node 版一致：banner 是页面上
    /// 的一块装饰，拉不到就不显示，不该让首屏出现一条红色报错。
    pub async fn get_activity_banner(&self, session: Option<&Value>) -> Value {
        let active = match session {
            Some(session) => Some(session.clone()),
            None => match self.require_session().await {
                Ok(session) => Some(session),
                Err(error) => {
                    logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
                    return Value::Null;
                }
            },
        };
        let result = match self
            .call_billing(
                ACTIVITY_BANNER,
                CallOptions { session: active.as_ref(), expect_code_ok: false, ..Default::default() },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
                return Value::Null;
            }
        };
        if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
            return Value::Null;
        }
        let raw = result.data;
        // action 只在 type 是 open_url / switch_model 时透出（其余类型前端不认识）。
        // 键的取舍照抄 Node 的对象字面量：`url` / `model_id` / `fallback_url`
        // 在缺失时是 undefined → **键整个消失**。实测上游的 open_url 就是
        // `{type, label, fallback_url}`（没有 url），所以响应里也不该有 url 键 ——
        // 前端 `action.url` 两种形态都读不到东西，但 `'url' in action` 会分叉。
        let action = raw.get("action").and_then(|action| {
            let action_type = action.get("type").and_then(Value::as_str).unwrap_or("");
            if action_type != "open_url" && action_type != "switch_model" {
                return None;
            }
            let mut object = Map::new();
            object.insert("type".to_string(), Value::String(action_type.to_string()));
            object.insert(
                "label".to_string(),
                action.get("label").cloned().unwrap_or(Value::Null),
            );
            if let Some(url) = action.get("url") {
                object.insert("url".to_string(), url.clone());
            }
            if let Some(model_id) = action.get("model_id") {
                object.insert("modelId".to_string(), model_id.clone());
            }
            if let Some(fallback) = action.get("fallback_url") {
                object.insert("fallbackUrl".to_string(), fallback.clone());
            }
            Some(Value::Object(object))
        });
        let mut banner = Map::new();
        banner.insert(
            "id".to_string(),
            raw.get("activity_id").cloned().unwrap_or(Value::Null),
        );
        // `raw.activity_online_status && raw.status === 'None'` ——
        // 前者是 JS 真值判定，后者是严格字符串比较
        banner.insert(
            "active".to_string(),
            Value::Bool(
                raw.get("activity_online_status").map(js_truthy).unwrap_or(false)
                    && raw.get("status").and_then(Value::as_str) == Some("None"),
            ),
        );
        banner.insert(
            "bannerContent".to_string(),
            raw.get("banner_content").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "link".to_string(),
            raw.get("detail_link").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "level".to_string(),
            raw.get("activity_level").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "startTime".to_string(),
            raw.get("start_time").cloned().unwrap_or(Value::Null),
        );
        banner.insert(
            "endTime".to_string(),
            raw.get("end_time").cloned().unwrap_or(Value::Null),
        );
        // Node 的 `let action` 在类型不认识时保持 undefined → 键整个消失；
        // 能识别时一定是个对象（上面已构造）
        if let Some(action) = action {
            banner.insert("action".to_string(), action);
        }
        banner.insert("raw".to_string(), raw);
        Value::Object(banner)
    }

    /// 大使（推广）状态。同样吞错返回 null（verbose 级别日志）。
    pub async fn get_ambassador_status(&self, session: Option<&Value>) -> Value {
        let active = match session {
            Some(session) => Some(session.clone()),
            None => match self.require_session().await {
                Ok(session) => Some(session),
                Err(error) => {
                    logging::verbose(
                        "[Activity]",
                        &format!("ambassador 状态拉取失败: {}", error.message),
                    );
                    return Value::Null;
                }
            },
        };
        let result = match self
            .call_billing(
                ACTIVITY_AMBASSADOR,
                CallOptions { session: active.as_ref(), expect_code_ok: false, ..Default::default() },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                logging::verbose(
                    "[Activity]",
                    &format!("ambassador 状态拉取失败: {}", error.message),
                );
                return Value::Null;
            }
        };
        if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
            return Value::Null;
        }
        json!({
            "isAmbassador": result
                .data
                .get("isAmbassador")
                .map(js_truthy)
                .unwrap_or(false),
            "raw": result.data,
        })
    }

    // ─── 组合动作 ───────────────────────────────────────────

    /// 签到 + 查余额的组合动作（"签到并回报最新积分"）。
    ///
    /// 已签到时不重复领取，直接返回当前额度。
    /// 国际版没有签到活动：不吞成「签到失败」，而是直接抛出，
    /// 让调用方拿到明确原因（`checkinStatus` 那一步也会先抛）。
    ///
    /// 并发语义：Node 用 `Promise.all` 并发跑「查状态 + 领取」。
    /// 这里必须**顺序**执行 —— 两者共享同一个底层连接池没问题，
    /// 但并发会在上游留下两条几乎同时到达的签到请求，而 Node 版
    /// `checkinAndReport` 的注释明确说「额度可能因签到变化，稍等一下再查」，
    /// 顺序执行才保证 `usage` 反映的是领取之后的额度。
    pub async fn checkin_and_report(
        &self,
        session: Option<&Value>,
        locale: Option<&str>,
    ) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        assert_checkin_supported(&active)?;

        // 查状态与领取：Node 并发，这里顺序（理由见上）。
        // 领取失败不抛出 —— 收敛成 `{success:false, code:-1, msg}`，与 Node 的
        // `.catch(error => ({ success:false, code:-1, msg: error.message }))` 一致
        let status = self.get_checkin_status(Some(&active)).await?;
        let claim = match self.claim_daily_checkin(Some(&active)).await {
            Ok(value) => value,
            Err(error) => json!({ "success": false, "code": -1, "msg": error.message }),
        };
        // 额度可能因签到变化，稍等一下再查（对照 Node 的 sleep(500)）
        if claim.get("success").and_then(Value::as_bool).unwrap_or(false) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let usage = match self.query_credits_summary(Some(&active), locale).await {
            Ok(value) => value,
            Err(error) => json!({ "error": error.message }),
        };
        Ok(json!({
            "checkinStatus": status,
            "claim": claim,
            "usage": usage,
        }))
    }
}
