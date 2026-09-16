/**
 * WorkBuddy 积分 / 计费 / 签到模块
 *
 * 逆向自 WorkBuddy 桌面端 v5.3.14（app.asar → main/workbuddy-auth-product-coordinator.js
 * 的 AuthService 类）。
 *
 * 端点（全部不带 prefixPath，直接挂 {endpoint}/v2/billing/meter/...）：
 *   POST /v2/billing/meter/checkin-activity-status      签到活动状态
 *   POST /v2/billing/meter/daily-checkin                领取每日签到积分
 *   POST /v2/billing/meter/get-user-resource            个人积分包（日额度/月会员/加油包…）
 *   POST /v2/billing/meter/get-enterprise-user-usage    企业额度
 *   POST /v2/billing/meter/get-dosage-notify            用量提示
 *   GET  /v2/activity/workbuddy/banner                  运营 banner（需客户端白名单头）
 *   GET  /v2/activity/ambassador/status                 大使状态
 *
 * 鉴权头（AuthService.buildHeaders）：
 *   Accept: application/json
 *   Content-Type: application/json
 *   Authorization: Bearer <accessToken>
 *   X-User-Id: <uid>
 *   X-Enterprise-Id / X-Tenant-Id: <enterpriseId>   （企业账号才带）
 *   X-Domain: <domain>
 *   Accept-Language: zh | en                         （计费接口按 UI 语言返回包名）
 *
 * 统一响应包 { code, data }，code === 0 为成功。
 */

import {
  BILLING,
  ACTIVITY,
  RESPONSE_CODE_OK,
  COMMODITY_CODES,
  COMMODITY_LABELS,
  DAILY_CREDITS,
  UNLIMITED_USAGE_SENTINEL,
  WORKBUDDY_CLIENT_VERSION,
  normalizeEndpoint,
  defaultPrefixPath,
  resolveEdition,
  userAgentForEdition,
} from './workbuddy-endpoints.mjs';
import { proxyFetch } from './workbuddy-proxy.mjs';

const REQUEST_TIMEOUT_MS = 20_000;

export class WorkBuddyBillingError extends Error {
  constructor(message, statusCode = 502, upstreamCode) {
    super(message);
    this.name = 'WorkBuddyBillingError';
    this.statusCode = statusCode;
    this.upstreamCode = upstreamCode;
  }
}

function toInt(value) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? Math.floor(parsed) : null;
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

export function createWorkBuddyBilling({
  auth,
  log = () => {},
  verbose = () => {},
  fetchImpl = globalThis.fetch,
  httpFetch = null,
  clientVersion = WORKBUDDY_CLIENT_VERSION,
} = {}) {
  if (!auth) throw new Error('createWorkBuddyBilling 需要传入 auth 提供者');

  /**
   * 出网函数，签名 (url, init, proxy)：默认用 workbuddy-proxy 的 proxyFetch
   * （undici，可按请求挂代理）；单测注入了替身 fetchImpl 时以替身为准。
   */
  const sendRequest = (fetchImpl && fetchImpl !== globalThis.fetch)
    ? fetchImpl
    : (httpFetch || proxyFetch);

  const baseUrl = normalizeEndpoint(auth.baseUrl);
  const prefix = auth.prefixPath ?? defaultPrefixPath(baseUrl);

  /** AuthService.buildHeaders(session) 的等价实现 */
  function buildHeaders(session, extra = {}) {
    const { auth: authPart, account } = session || {};
    const headers = {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      Authorization: `Bearer ${authPart.accessToken}`,
      'X-User-Id': account?.uid || '',
      ...extra,
    };
    if (account?.enterpriseId) {
      headers['X-Enterprise-Id'] = account.enterpriseId;
      headers['X-Tenant-Id'] = account.enterpriseId;
    }
    if (authPart?.domain) headers['X-Domain'] = authPart.domain;
    return headers;
  }

  /** 运营活动接口需要的客户端白名单头（getActivityBanner 专用）；按账号版本区分身份 */
  function whitelistHeaders(session) {
    const e = resolveEdition(session?.edition);
    return {
      'User-Agent': userAgentForEdition(e.id),
      'X-IDE-Type': e.uaPlatform,
      'X-IDE-Name': e.productName,
      'X-IDE-Version': e.clientVersion,
      'X-Product': e.productName,
    };
  }

  async function requireSession() {
    const session = await auth.getCurrentSession();
    if (!session?.auth?.accessToken) {
      throw new WorkBuddyBillingError(
        '当前没有可用登录态：先执行 node server.mjs --login，或设置 WORKBUDDY_TOKEN',
        401,
      );
    }
    return session;
  }

  /**
   * 调用计费/活动接口。返回 { data, code, msg, requestId }。
   * expectCodeOk=false 时非 0 code 也返回（签到重复领取要读 msg）。
   * session.proxy 为该账号的出网代理（多账号时由调用方从 store 带入），
   * 缺失则直连 —— 与 LLM 转发保持同一出口。
   */
  async function callBilling(spec, { body, session, query = '', expectCodeOk = true, locale } = {}) {
    const activeSession = session || await requireSession();
    // 账户级端点：国内版/国际版账号各走自己的站点
    const activeBase = normalizeEndpoint(activeSession.endpoint || baseUrl);
    const url = `${activeBase}${spec.path}${query}`;
    const extra = {};
    if (locale) extra['Accept-Language'] = locale;
    if (spec.whitelistHeaders) Object.assign(extra, whitelistHeaders(activeSession));

    const init = {
      method: spec.method,
      headers: buildHeaders(activeSession, extra),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    };
    if (spec.method !== 'GET' && spec.method !== 'HEAD') {
      init.body = JSON.stringify(body ?? spec.body ?? {});
    }

    const proxy = activeSession.proxy || null;
    verbose('[Billing]', `${spec.method} ${url}${proxy ? ` 经代理 ${proxy.label || proxy.host}` : ''}`);
    let response;
    try {
      response = await sendRequest(url, init, proxy);
    } catch (error) {
      if (error?.name === 'AbortError' || error?.name === 'TimeoutError') {
        throw new WorkBuddyBillingError('计费接口请求超时', 504);
      }
      throw new WorkBuddyBillingError(`计费接口请求失败: ${error instanceof Error ? error.message : String(error)}`);
    }

    const text = await response.text().catch(() => '');
    let payload = null;
    try { payload = text ? JSON.parse(text) : null; } catch { payload = null; }

    if (response.status === 401 || response.status === 403) {
      throw new WorkBuddyBillingError('登录态已过期或被拒绝，无法调用计费接口', 401, payload?.code);
    }
    if (!response.ok) {
      throw new WorkBuddyBillingError(
        `计费接口返回 HTTP ${response.status}${payload?.msg || payload?.message ? `: ${payload.msg || payload.message}` : ''}`,
        response.status,
        payload?.code,
      );
    }
    if (payload && typeof payload.code === 'number' && payload.code !== RESPONSE_CODE_OK && expectCodeOk) {
      throw new WorkBuddyBillingError(
        `${payload.msg || payload.message || `计费接口返回 code=${payload.code}`}`,
        response.status,
        payload.code,
      );
    }
    return {
      code: payload?.code,
      msg: payload?.msg || payload?.message,
      requestId: payload?.requestId,
      data: payload?.data ?? null,
      raw: payload,
    };
  }

  // ─── 签到 ───────────────────────────────────────────────

  /**
   * 国际版（www.workbuddy.ai）目前没有签到活动，调用上游只会拿到无意义的结果。
   * 这里统一拦截，避免各入口（CLI / HTTP / 桌面端）分别判断漏掉。
   */
  function assertCheckinSupported(session) {
    if (resolveEdition(session?.edition).id === 'intl') {
      throw new WorkBuddyBillingError('国际版账号暂无签到活动', 400);
    }
  }

  /**
   * 签到活动状态（AuthService.getCheckinStatus）。
   * 成功但 data 为空时返回 null（与桌面端一致）。
   */
  async function getCheckinStatus({ session } = {}) {
    const activeSession = session || await requireSession();
    assertCheckinSupported(activeSession);
    const result = await callBilling(BILLING.checkinStatus, {
      session: activeSession,
      expectCodeOk: false,
    });
    if (result.code !== RESPONSE_CODE_OK || !result.data) return null;
    return normalizeCheckin(result.data);
  }

  /**
   * 领取每日签到积分（AuthService.claimDailyCheckin）。
   * 幂等：已领取时上游返回非 0 code，这里原样返回 { success:false, code, msg }。
   */
  async function claimDailyCheckin({ session } = {}) {
    const activeSession = session || await requireSession();
    assertCheckinSupported(activeSession);
    const result = await callBilling(BILLING.dailyCheckin, {
      session: activeSession,
      expectCodeOk: false,
    });
    if (result.code === RESPONSE_CODE_OK && result.data) {
      return { success: true, code: 0, data: normalizeCheckin(result.data), raw: result.data };
    }
    return {
      success: false,
      code: result.code ?? -1,
      msg: result.msg || '签到失败',
      requestId: result.requestId,
    };
  }

  /**
   * 签到结果归一化。上游字段随活动配置变化，这里做「宽进」处理：
   * 保留原始 data，同时尽力抽出常见字段供 UI 直接展示。
   */
  function normalizeCheckin(data) {
    if (!data || typeof data !== 'object') return data;
    const pick = (...keys) => {
      for (const key of keys) {
        if (data[key] !== undefined && data[key] !== null) return data[key];
      }
      return undefined;
    };
    return {
      // 今日是否已签到
      checkedIn: pick('checked_in', 'checkedIn', 'is_checked_in', 'isCheckedIn', 'signed', 'is_signed'),
      // 连续签到天数
      continuousDays: toInt(pick('continuous_days', 'continuousDays', 'continuous_checkin_days', 'streak', 'serial_days')),
      // 累计签到天数
      totalDays: toInt(pick('total_days', 'totalDays', 'total_checkin_days', 'accumulate_days')),
      // 本次/今日可领积分
      points: toInt(pick('points', 'credit', 'reward_points', 'rewardPoints', 'daily_points')),
      // 活动周期
      startTime: pick('start_time', 'startTime'),
      endTime: pick('end_time', 'endTime'),
      // 活动是否在线
      online: pick('activity_online_status', 'activityOnlineStatus', 'online'),
      raw: data,
    };
  }

  // ─── 积分查询 ───────────────────────────────────────────

  /**
   * 查询账号积分/额度。企业账号走企业接口，个人账号走资源接口。
   * 返回统一结构 { kind, usageLeft, usageTotal, usageUsed, editionType, resources[], raw }。
   */
  async function queryUsage({ session, locale } = {}) {
    const activeSession = session || await requireSession();
    const isEnterprise = Boolean(activeSession.account?.enterpriseId);
    return isEnterprise
      ? getEnterpriseUsage(activeSession)
      : getPersonalUsage(activeSession, { locale });
  }

  /** 个人账号：POST /v2/billing/meter/get-user-resource */
  async function getPersonalUsage(session, { locale } = {}) {
    const result = await callBilling(BILLING.userResource, {
      session,
      locale: resolveAcceptLanguage(locale),
    });
    // 结果路径：data.Response.Data.Accounts[]
    const accounts = result.data?.Response?.Data?.Accounts;
    const resources = Array.isArray(accounts) ? accounts : [];

    const planResources = resources.map(r => {
      const isDaily = DAILY_CREDITS.includes(r.PackageCode);
      const total = Number(r.CycleCapacitySizePrecise) || 0;
      const left = Number(r.CycleCapacityRemainPrecise) || 0;
      return {
        id: r.ResourceId,
        packageCode: r.PackageCode,
        name: COMMODITY_LABELS[r.PackageCode] || r.PackageName || r.PackageCode || '',
        isDaily,
        total,
        used: Math.max(0, total - left),
        left,
        startAt: parseTime(r.DeductionStartTime) || parseTime(r.CycleStartTime) || null,
        expireAt: parseTime(isDaily ? r.CycleEndTime : r.DeductionEndTime) || null,
        refreshAt: isDaily ? null : (parseTime(r.CycleEndTime) || 0) + 1000 || null,
        autoRenew: Number(r.AutoRenewFlag) === 1,
      };
    }).sort((a, b) => {
      // 与桌面端一致：先按套餐优先级，同优先级再按到期时间升序
      const diff = planPriority(a.packageCode) - planPriority(b.packageCode);
      if (diff !== 0) return diff;
      return (a.expireAt ?? Infinity) - (b.expireAt ?? Infinity);
    });

    const usageTotal = planResources.reduce((sum, r) => sum + r.total, 0);
    const usageLeft = planResources.reduce((sum, r) => sum + r.left, 0);
    const usageUsed = planResources.reduce((sum, r) => sum + r.used, 0);

    // 当前生效套餐按等级挑选（旗舰 > 进阶 > 专业 > 青春 > 试用），
    // 而不是简单取列表首个（日额度包金额小但到期最晚，会误判为 free）
    const findCode = (...codes) => planResources.find(r => codes.includes(r.packageCode));
    const flagshipPlan = findCode(COMMODITY_CODES.flagship);
    const advancedPlan = findCode(COMMODITY_CODES.advanced);
    const proPlan = findCode(COMMODITY_CODES.proYear, COMMODITY_CODES.proMon, COMMODITY_CODES.proMonPlus);
    const youthPlan = findCode(COMMODITY_CODES.youth);
    const trialPlan = findCode(
      COMMODITY_CODES.proTrialMon, COMMODITY_CODES.proTrialYear,
      COMMODITY_CODES.freeMonIntl, COMMODITY_CODES.gift, COMMODITY_CODES.freeMon,
    );
    const active = flagshipPlan || advancedPlan || proPlan || youthPlan || trialPlan || planResources[0] || null;

    return {
      kind: 'personal',
      editionType: editionFromPlans({ flagshipPlan, advancedPlan, proPlan, youthPlan, trialPlan }),
      isPro: !!proPlan,
      isTrial: !!trialPlan && !proPlan && !advancedPlan && !flagshipPlan && !youthPlan,
      planName: active?.name || '',
      packageCode: active?.packageCode || null,
      usageTotal: String(usageTotal),
      usageLeft: String(usageLeft),
      usageUsed: String(usageUsed),
      expireAt: active?.expireAt || null,
      refreshAt: active?.refreshAt || null,
      autoRenew: active?.autoRenew ?? false,
      resources: planResources,
      raw: result.data,
    };
  }

  /** 企业账号：POST /v2/billing/meter/get-enterprise-user-usage */
  async function getEnterpriseUsage(session) {
    const result = await callBilling(BILLING.enterpriseUsage, { session });
    const responseData = result.raw ?? {};
    const usageData = responseData?.data?.data || responseData?.data || result.data || responseData;
    const editionType = session.account?.type === 'ultimate' ? 'ultimate'
      : session.account?.type === 'exclusive' ? 'exclusive' : 'enterprise';

    if (usageData && typeof usageData.limitNum === 'number') {
      const credit = typeof usageData.credit === 'number' ? usageData.credit : 0;
      // limitNum === -1 表示不限量
      if (usageData.limitNum === -1) {
        return {
          kind: 'enterprise',
          editionType,
          unlimited: true,
          usageLeft: UNLIMITED_USAGE_SENTINEL,
          usageTotal: UNLIMITED_USAGE_SENTINEL,
          usageUsed: String(credit),
          refreshAt: usageData.cycleResetTime ? parseTime(usageData.cycleResetTime) : null,
          resources: [],
          raw: usageData,
        };
      }
      return {
        kind: 'enterprise',
        editionType,
        unlimited: false,
        usageLeft: String(usageData.limitNum - credit),
        usageTotal: String(usageData.limitNum),
        usageUsed: String(credit),
        refreshAt: usageData.cycleResetTime ? parseTime(usageData.cycleResetTime) : null,
        resources: [],
        raw: usageData,
      };
    }
    return {
      kind: 'enterprise',
      editionType,
      unlimited: false,
      usageLeft: '0',
      usageTotal: '0',
      usageUsed: '0',
      resources: [],
      raw: usageData ?? null,
    };
  }

  /** 轻量余额查询：只取剩余额度，供限额判定等热路径使用 */
  async function fetchAvailableCredits({ session } = {}) {
    const usage = await queryUsage({ session });
    if (usage.unlimited) return Infinity;
    const left = toInt(usage.usageLeft);
    if (left === null) throw new WorkBuddyBillingError('积分响应缺少可解析的剩余量');
    return left;
  }

  // ─── 积分简报 ───────────────────────────────────────────

  /** 订阅套餐自带的基础积分包（旗舰/进阶/青春/专业/试用体验/免费额度） */
  const PLAN_BASE_PACKAGES = new Set([
    COMMODITY_CODES.flagship, COMMODITY_CODES.advanced, COMMODITY_CODES.youth,
    COMMODITY_CODES.proMon, COMMODITY_CODES.proYear, COMMODITY_CODES.proMonPlus,
    COMMODITY_CODES.proTrialMon, COMMODITY_CODES.proTrialYear, COMMODITY_CODES.gift,
    COMMODITY_CODES.free, COMMODITY_CODES.freeMon, COMMODITY_CODES.freeMonIntl,
  ]);
  /** 平台奖励积分包（成长计划/运营赠送） */
  const BONUS_PACKAGES = new Set([
    COMMODITY_CODES.activity,
    COMMODITY_CODES.bonus28, COMMODITY_CODES.bonus29, COMMODITY_CODES.bonus30,
    COMMODITY_CODES.bonusIntl,
  ]);

  /**
   * 积分简报：只返回三个数 —— 总剩余积分 / 套餐基础积分 / 平台奖励积分。
   * 总剩余含加油包等全部资源包，因此可能大于后两项之和（买了加油包时）。
   * 企业账号没有套餐/奖励之分：totalLeft 即剩余额度，后两项为 null（不限量时为 null + unlimited 标记）。
   */
  async function queryCreditsSummary({ session, locale } = {}) {
    const usage = await queryUsage({ session, locale });
    if (usage.kind === 'enterprise') {
      return {
        kind: 'enterprise',
        unlimited: !!usage.unlimited,
        totalLeft: usage.unlimited ? null : toInt(usage.usageLeft),
        planLeft: null,
        bonusLeft: null,
      };
    }
    let totalLeft = 0;
    let planLeft = 0;
    let bonusLeft = 0;
    for (const r of usage.resources ?? []) {
      const left = toInt(r.left) ?? 0;
      totalLeft += left;
      if (PLAN_BASE_PACKAGES.has(r.packageCode)) planLeft += left;
      else if (BONUS_PACKAGES.has(r.packageCode)) bonusLeft += left;
    }
    return { kind: 'personal', unlimited: false, totalLeft, planLeft, bonusLeft };
  }

  /** 用量提示（productFeatures.BillingNotice 开启时桌面端会轮询） */
  async function getDosageNotify({ session } = {}) {
    const result = await callBilling(BILLING.dosageNotify, { session, expectCodeOk: false });
    return result.code === RESPONSE_CODE_OK ? result.data : null;
  }

  // ─── 活动 ───────────────────────────────────────────────

  /** 运营 banner：需客户端白名单头，否则被拦截（返回 null，与桌面端一致） */
  async function getActivityBanner({ session } = {}) {
    try {
      const result = await callBilling(ACTIVITY.workbuddyBanner, {
        session: session || await requireSession(),
        expectCodeOk: false,
      });
      if (result.code !== RESPONSE_CODE_OK || !result.data) return null;
      const raw = result.data;
      let action;
      if (raw.action?.type === 'open_url' || raw.action?.type === 'switch_model') {
        action = {
          type: raw.action.type,
          label: raw.action.label,
          url: raw.action.url,
          modelId: raw.action.model_id,
          fallbackUrl: raw.action.fallback_url,
        };
      }
      return {
        id: raw.activity_id,
        active: raw.activity_online_status && raw.status === 'None',
        bannerContent: raw.banner_content,
        link: raw.detail_link,
        level: raw.activity_level,
        startTime: raw.start_time,
        endTime: raw.end_time,
        action,
        raw,
      };
    } catch (error) {
      log('[Activity]', `banner 拉取失败: ${error.message}`);
      return null;
    }
  }

  /** 大使（推广）状态 */
  async function getAmbassadorStatus({ session } = {}) {
    try {
      const result = await callBilling(ACTIVITY.ambassadorStatus, {
        session: session || await requireSession(),
        expectCodeOk: false,
      });
      if (result.code !== RESPONSE_CODE_OK || !result.data) return null;
      return { isAmbassador: !!result.data.isAmbassador, raw: result.data };
    } catch (error) {
      verbose('[Activity]', `ambassador 状态拉取失败: ${error.message}`);
      return null;
    }
  }

  // ─── 工具函数 ───────────────────────────────────────────

  /** 计费接口的 Accept-Language：zh | en（缺省让后端走 default） */
  function resolveAcceptLanguage(locale) {
    if (locale) {
      const normalized = String(locale).toLowerCase();
      if (normalized.startsWith('zh')) return 'zh';
      if (normalized.startsWith('en')) return 'en';
    }
    return 'zh';
  }

  function parseTime(time) {
    if (!time) return 0;
    if (typeof time === 'string' && /^\d+$/.test(time)) return Number(time);
    const parsed = new Date(time).getTime();
    return Number.isFinite(parsed) ? parsed : 0;
  }

  /**
   * 套餐优先级（与桌面端 getPriority 一致）：
   *   1 订阅/试用类  2 赠送 28 类  3 加油包  4 活动/赠送 29-30  5 礼包  6 免费日额度  7 其他
   */
  function planPriority(code) {
    if ([
      COMMODITY_CODES.proMon, COMMODITY_CODES.proMonPlus, COMMODITY_CODES.proYear,
      COMMODITY_CODES.youth, COMMODITY_CODES.advanced, COMMODITY_CODES.flagship,
      COMMODITY_CODES.freeMonIntl, COMMODITY_CODES.proTrialMon, COMMODITY_CODES.proTrialYear,
      COMMODITY_CODES.freeMon,
    ].includes(code)) return 1;
    if ([COMMODITY_CODES.bonus28, COMMODITY_CODES.bonusIntl].includes(code)) return 2;
    if ([COMMODITY_CODES.extra, COMMODITY_CODES.extra38, COMMODITY_CODES.extraIntl].includes(code)) return 3;
    if ([
      COMMODITY_CODES.activity, COMMODITY_CODES.bonus29, COMMODITY_CODES.bonus30,
      COMMODITY_CODES.gift,
    ].includes(code)) return 4;
    if (COMMODITY_CODES.free === code) return 6;
    return 7;
  }

  /** 按已识别的套餐等级推导展示类型（与桌面端 getEditionDisplayType 对齐） */
  function editionFromPlans({ flagshipPlan, advancedPlan, proPlan, youthPlan, trialPlan }) {
    if (flagshipPlan) return 'flagship';
    if (advancedPlan) return 'advanced';
    if (youthPlan && !proPlan) return 'youth';
    if (proPlan) return 'pro';
    if (trialPlan) return 'pro_trial';
    return 'free';
  }

  /**
   * 签到 + 查余额的组合动作（"签到并回报最新积分"）。
   * 已签到时不重复领取，直接返回当前额度。
   * 国际版没有签到活动：不吞成「签到失败」，而是直接抛出，让调用方拿到明确原因。
   */
  async function checkinAndReport({ session, locale } = {}) {
    const activeSession = session || await requireSession();
    assertCheckinSupported(activeSession);
    const [status, claim] = await Promise.all([
      getCheckinStatus({ session: activeSession }),
      claimDailyCheckin({ session: activeSession }).catch(error => ({
        success: false, code: -1, msg: error.message,
      })),
    ]);
    // 额度可能因签到变化，稍等一下再查
    if (claim.success) await sleep(500);
    const usage = await queryCreditsSummary({ session: activeSession, locale }).catch(error => ({
      error: error.message,
    }));
    return { checkinStatus: status, claim, usage };
  }

  return {
    getCheckinStatus,
    claimDailyCheckin,
    queryUsage,
    queryCreditsSummary,
    getPersonalUsage,
    getEnterpriseUsage,
    fetchAvailableCredits,
    getDosageNotify,
    getActivityBanner,
    getAmbassadorStatus,
    checkinAndReport,
    buildHeaders,
    get baseUrl() { return baseUrl; },
    get prefixPath() { return prefix; },
  };
}
