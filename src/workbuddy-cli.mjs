/**
 * 命令行子命令与启动自检输出。
 *
 * 这些分支只在「不启动 HTTP 服务」时执行（--endpoints / --status / --usage /
 * --checkin / --logout / --login），以及 serve 模式启动前的配置自检打印。
 * 与请求处理链路无关，单独成文件让 server.mjs 专注在服务装配上。
 */

import { EDITIONS, resolveEdition } from './workbuddy-endpoints.mjs';
import { MODEL_ALLOWLIST } from './workbuddy-models.mjs';
import { clashProxyOptions } from './workbuddy-proxy.mjs';

/** 积分简报（--usage / --checkin 共用的终端输出） */
export function printUsageReport(usage) {
  console.log('');
  if (usage?.unlimited) {
    console.log('  总剩余积分   : 不限量');
    console.log('');
    return;
  }
  console.log(`  总剩余积分   : ${usage?.totalLeft ?? '-'}`);
  console.log(`  套餐基础积分 : ${usage?.planLeft ?? '-'}`);
  console.log(`  平台奖励积分 : ${usage?.bonusLeft ?? '-'}`);
  console.log('');
}

/** --endpoints：把逆向出的接口清单按分组打印 */
export async function runEndpointsCommand({ auth, edition }) {
  const { AUTH, LLM, BILLING, ACTIVITY, CONFIG, CLOUD_AGENT } = await import('./workbuddy-endpoints.mjs');
  const render = (title, group) => {
    console.log(`\n══ ${title} ══`);
    for (const [key, spec] of Object.entries(group)) {
      // 带 enterpriseId 等占位的路径用 {placeholder} 展示，避免打印 undefined
      const p = (typeof spec.path !== 'function'
        ? spec.path
        : (spec.path.length > 1 ? spec.path(auth.prefixPath, '{enterpriseId}') : spec.path(auth.prefixPath))
      ).replace(/%7B/gi, '{').replace(/%7D/gi, '}');
      console.log(`  ${String(spec.method).padEnd(5)} ${p}${spec.query ? `?${Object.keys(spec.query).join('=')}` : ''}   [${key}]`);
      if (spec.note) console.log(`        └ ${spec.note}`);
    }
  };
  console.log(`端点: ${auth.baseUrl}   鉴权前缀: ${JSON.stringify(auth.prefixPath)}   platform: ${auth.platform}   版本: ${resolveEdition(edition).label}`);
  for (const e of Object.values(EDITIONS)) {
    console.log(`  [${e.id}] ${e.label}: ${e.endpoint}   platform=${e.platform}   UA 产品名=${e.productName}`);
  }
  render('鉴权 / 账号（带前缀）', AUTH);
  render('LLM（不带前缀）', LLM);
  render('计费 / 积分 / 签到（不带前缀）', BILLING);
  render('活动 / 用户', ACTIVITY);
  render('配置 / 模型', CONFIG);
  render('云智能体', CLOUD_AGENT);
  console.log('');
}

/** --status：打印登录态 JSON（未登录时由调用方决定退出码） */
export async function runStatusCommand({ auth }) {
  const status = await auth.getStatus();
  console.log(JSON.stringify(status, null, 2));
  return status;
}

/** --checkin：签到 + 打印最新积分 */
export async function runCheckinCommand({ billing, locale }) {
  const result = await billing.checkinAndReport({ locale });
  if (result.checkinStatus) {
    const s = result.checkinStatus;
    console.log('');
    console.log(`  今日已签到 : ${s.checkedIn === undefined ? '未知' : (s.checkedIn ? '是' : '否')}`);
    if (s.continuousDays !== undefined) console.log(`  连续天数   : ${s.continuousDays}`);
    if (s.totalDays !== undefined) console.log(`  累计天数   : ${s.totalDays}`);
  }
  console.log(`  签到结果   : ${result.claim.success ? '✅ 领取成功' : `❌ ${result.claim.msg}（code=${result.claim.code}）`}`);
  if (result.usage?.error) console.log(`  积分查询   : ⚠️ ${result.usage.error}`);
  else printUsageReport(result.usage);
}

/**
 * serve 模式启动自检：把端口/账号路由顺序/出网代理/Clash/脱敏状态打一遍，
 * 出问题时用户能直接从这段日志看出配置是否符合预期。
 */
export function logStartupConfig({ opts, auth, store, desensitizer, log }) {
  console.log('');
  console.log('╔══════════════════════════════════════════════════════════╗');
  console.log('║       WorkBuddy Local Proxy                              ║');
  console.log('║       OpenAI 兼容 · WorkBuddy 登录态复用 · 积分/签到      ║');
  console.log('╚══════════════════════════════════════════════════════════╝');
  console.log('');

  log('[Config]', `API 端口: ${opts.port}`);
  log('[Config]', `API 监听地址: ${opts.host}`);
  log('[Config]', `API Key 认证: ${opts.apiKey ? '✅ 已启用' : '❌ 未启用'}`);
  log('[Config]', `上游端点: ${auth.baseUrl}（鉴权前缀 ${JSON.stringify(auth.prefixPath)}，默认版本 ${resolveEdition(opts.edition).label}）`);
  log('[Config]', '账号版本: 每个账号自带 edition（国内版 copilot.tencent.com / 国际版 www.workbuddy.ai），刷新与转发按账号记录走');
  log('[Config]', `默认模型: ${opts.defaultModel}（不在目录时回退目录内 isDefault 模型；点名未知模型一律 400 报错）`);
  log('[Config]', `模型白名单: ${MODEL_ALLOWLIST.length ? `${MODEL_ALLOWLIST.join('、')}（/v1/models 与路由只暴露这些模型）` : '未启用（/v1/models 暴露 /v3/config 全量清单）'}`);
  log('[Config]', `计费语言: ${opts.locale}`);

  const snapshot = store.listAccounts();
  const enabled = snapshot.accounts.filter(a => a.enabled !== false);
  const disabled = snapshot.accounts.length - enabled.length;
  const withProxy = enabled.filter(a => a.proxy && !a.proxy.error);
  const brokenProxy = enabled.filter(a => a.proxy?.error);
  log('[Config]', `账号路由: 严格按优先级升序（优先级全局唯一），当前 ${enabled.length} 个启用`
    + `${disabled ? `、${disabled} 个已禁用（不参与转发）` : ''}`);
  if (enabled.length) {
    const plan = enabled
      .slice()
      .sort((a, b) => a.priority - b.priority || a.addedAt - b.addedAt)
      .map(a => `${a.name}(P${a.priority})`)
      .join(' → ');
    log('[Config]', `转发顺序: ${plan}`);
  }
  log('[Config]', `出网代理: ${withProxy.length ? `${withProxy.length} 个账号走代理` : '全部账号直连（无代理）'}`
    + `${brokenProxy.length ? `；${brokenProxy.length} 个账号代理不可用（将回退直连）` : ''}`);
  const clash = clashProxyOptions();
  log('[Config]', clash.available
    ? `Clash Verge: ✅ 已检测到（${clash.options.length} 个可用出口${clash.dir ? `，${clash.dir}` : ''}）`
    : `Clash Verge: 未检测到（${clash.error}）`);

  const s = desensitizer.getState();
  log('[Config]', `内容脱敏: ${s.enabled ? '✅ 已启用' : '❌ 已关闭'}`
    + `（${s.termCount} 个词，作用角色 ${s.roles.join('、')}，词表 ${s.file}）`
    + (opts.desensitize !== undefined ? '（本次运行由命令行/环境变量覆盖）' : ''));
}
