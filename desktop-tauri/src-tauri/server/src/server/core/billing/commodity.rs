//! 积分包商品码与套餐分类（对照 Node 版 workbuddy-endpoints.mjs 的
//! COMMODITY_CODES / COMMODITY_LABELS / DAILY_CREDITS 与 workbuddy-billing.mjs
//! 里的 planPriority / PLAN_BASE_PACKAGES / BONUS_PACKAGES）。
//!
//! 为什么放在 billing 子模块而不是 endpoints.rs：这些常量只服务于「积分查询结果
//! 的解读」，端点表那边一个都用不到；混在 endpoints.rs 里会让那个「接口清单
//! 唯一事实来源」的文件掺进业务语义。`/api/endpoints` 不需要输出它们。

/// 积分包商品码（账单里 PackageCode 的含义）
pub mod codes {
    pub const FREE: &str = "TCACA_code_001_PqouKr6QWV";
    pub const PRO_MON: &str = "TCACA_code_002_AkiJS3ZHF5";
    pub const PRO_YEAR: &str = "TCACA_code_003_FAnt7lcmRT";
    pub const PRO_MON_PLUS: &str = "TCACA_code_005_maRGyrHhw1";
    pub const GIFT: &str = "TCACA_code_006_DbXS0lrypC";
    pub const ACTIVITY: &str = "TCACA_code_007_nzdH5h4Nl0";
    pub const FREE_MON: &str = "TCACA_code_008_cfWoLwvjU4";
    pub const EXTRA: &str = "TCACA_code_009_0XmEQc2xOf";
    pub const YOUTH: &str = "TCACA_code_023_4xbGhMrE6q";
    pub const ADVANCED: &str = "TCACA_code_026_BaESVICNoi";
    pub const FLAGSHIP: &str = "TCACA_code_027_0FCGVA6vSa";
    pub const BONUS28: &str = "TCACA_code_028_NtpWi0jzXs";
    pub const BONUS29: &str = "TCACA_code_029_6wCGEWquYy";
    pub const BONUS30: &str = "TCACA_code_030_BjSt89qTvr";
    pub const FREE_MON_INTL: &str = "TCACA_code_035_ArVxJcGDsm";
    pub const EXTRA_INTL: &str = "TCACA_code_036_lupO5WgNdG";
    pub const BONUS_INTL: &str = "TCACA_code_037_WxOD3MpI2o";
    pub const EXTRA38: &str = "TCACA_code_038_OhvqZtiPKr";
    pub const PRO_TRIAL_MON: &str = "TCACA_code_039_KRcQj7wUat";
    pub const PRO_TRIAL_YEAR: &str = "TCACA_code_040_mi9rCYg46x";
}

/// 商品码 → 展示名（前端明细里的「套餐」一栏直接用这个）
pub fn label_of(code: &str) -> Option<&'static str> {
    use codes::*;
    Some(match code {
        FREE => "免费版每日额度",
        PRO_MON => "专业版包月",
        PRO_YEAR => "专业版包年",
        PRO_MON_PLUS => "专业版包月（升级包）",
        GIFT => "专业版体验",
        ACTIVITY => "成长计划",
        FREE_MON => "专业版日额度",
        EXTRA => "积分加油包",
        YOUTH => "青春版",
        ADVANCED => "进阶版",
        FLAGSHIP => "旗舰版",
        BONUS28 => "赠送积分（28）",
        BONUS29 => "赠送积分（29）",
        BONUS30 => "赠送积分（30）",
        FREE_MON_INTL => "国际版月额度",
        EXTRA_INTL => "国际版加油包",
        BONUS_INTL => "国际版赠送积分",
        EXTRA38 => "积分加油包（38）",
        PRO_TRIAL_MON => "专业版试用（月）",
        PRO_TRIAL_YEAR => "专业版试用（年）",
        _ => return None,
    })
}

/// 按日刷新的积分包（额度每天重置，无过期时间）
pub fn is_daily_credit(code: &str) -> bool {
    code == codes::FREE
}

/// 企业不限量哨兵（与桌面端 UNLIMITED_USAGE_SENTINEL 一致）
pub const UNLIMITED_USAGE_SENTINEL: &str = "unlimited";

/// 套餐优先级（与桌面端 getPriority 一致）：
///   1 订阅/试用类  2 赠送 28 类  3 加油包  4 活动/赠送 29-30/礼包  6 免费日额度  7 其他
///
/// 列表按这个优先级 + 到期时间升序排，前端从上往下读第一个「当前生效套餐」
/// （所以日额度包即使金额小、到期最晚，也不会被误判成免费版）。
pub fn plan_priority(code: Option<&str>) -> i32 {
    use codes::*;
    let Some(code) = code else {
        return 7;
    };
    if [
        PRO_MON, PRO_MON_PLUS, PRO_YEAR, YOUTH, ADVANCED, FLAGSHIP, FREE_MON_INTL,
        PRO_TRIAL_MON, PRO_TRIAL_YEAR, FREE_MON,
    ]
    .contains(&code)
    {
        return 1;
    }
    if [BONUS28, BONUS_INTL].contains(&code) {
        return 2;
    }
    if [EXTRA, EXTRA_INTL, EXTRA38].contains(&code) {
        return 3;
    }
    if [ACTIVITY, BONUS29, BONUS30, GIFT].contains(&code) {
        return 4;
    }
    if code == FREE {
        return 6;
    }
    7
}

/// 订阅套餐自带的基础积分包（旗舰/进阶/青春/专业/试用体验/免费额度）
pub fn is_plan_base(code: Option<&str>) -> bool {
    use codes::*;
    let Some(code) = code else {
        return false;
    };
    [
        FLAGSHIP, ADVANCED, YOUTH, PRO_MON, PRO_YEAR, PRO_MON_PLUS, PRO_TRIAL_MON,
        PRO_TRIAL_YEAR, GIFT, FREE, FREE_MON, FREE_MON_INTL,
    ]
    .contains(&code)
}

/// 平台奖励积分包（成长计划/运营赠送）
pub fn is_bonus(code: Option<&str>) -> bool {
    use codes::*;
    let Some(code) = code else {
        return false;
    };
    [ACTIVITY, BONUS28, BONUS29, BONUS30, BONUS_INTL].contains(&code)
}
