/**
 * WorkBuddy 多账号管理路由（对齐 raccoon-local-proxy 的 /api/accounts 风格）
 *
 *   GET    /api/accounts                账号列表（含当前账号标记、优先级、启用状态、代理）
 *   POST   /api/accounts                手动添加账号（accessToken/refreshToken JSON）
 *   GET    /api/accounts/export         导出全部账号（含 token，换机器后导入继续用）
 *   POST   /api/accounts/import         导入账号（merge：按 uid 匹配，命中更新、未命中追加）
 *   POST   /api/accounts/current        把账号置顶（即切换当前账号）{ id }
 *   PATCH  /api/accounts/<id>           修改账号属性 { name?, priority?, enabled?, proxy? }
 *   POST   /api/accounts/<id>/move      与相邻账号交换优先级 { direction: 'up' | 'down' }
 *   POST   /api/accounts/batch          批量修改 { action: 'enable'|'disable'|'proxy'|'remove', ids, proxy? }
 *   POST   /api/accounts/refresh        刷新指定（或当前）账号的 token { id? }
 *   GET    /api/accounts/usage          批量查询各账号积分/额度
 *   POST   /api/accounts/checkin        批量/单个签到 { id? }
 *   DELETE /api/accounts/<id>           删除账号
 *
 *   GET    /api/proxies                 出网代理可选项（Clash Verge 监听器 + 全局混合端口）
 *   POST   /api/proxies/test            测试出网（{ proxy } 或 { id } 用账号的代理）
 *
 * tryHandle 返回 true 表示请求已被处理，否则交给后续路由。
 */

import { AccountStoreError, normalizePriority } from './workbuddy-account-store.mjs';
import { WorkBuddyAuthError } from './workbuddy-auth.mjs';
import {
  ProxyConfigError,
  clashProxyOptions,
  normalizeAccountProxy,
  resolveAccountProxy,
  testProxyConnectivity,
} from './workbuddy-proxy.mjs';

function sendJson(res, status, payload) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(payload));
}

export function setupAccountRoutes({
  store,
  auth,
  billing,
  log = () => {},
  verbose = () => {},
  checkApiKey = () => true,
  unauthorized = (_res) => {},
  readRawBody,
}) {
  async function refreshById(id) {
    const creds = store.getCredentialsById(id);
    if (!creds) throw new AccountStoreError('账号不存在', 404);
    if (!creds.refreshToken) {
      throw new AccountStoreError(`账号 ${creds.name} 没有 refreshToken，无法刷新`);
    }
    // 按账号自己的端点续期（国内版 / 国��版不同站点），出口也按账号的代理走
    const nextAuth = await auth.refreshSession({
      accessToken: creds.accessToken,
      refreshToken: creds.refreshToken,
      expiresAt: creds.expiresAt,
    }, auth.makeContext({
      endpoint: creds.endpoint,
      prefixPath: creds.prefixPath,
      platform: creds.platform,
      edition: creds.edition,
    }), creds.proxy || null);
    if (!nextAuth?.accessToken) throw new WorkBuddyAuthError('token 刷新响应缺少 accessToken');
    store.updateAccountTokens(id, {
      accessToken: nextAuth.accessToken,
      refreshToken: nextAuth.refreshToken,
      expiresAt: nextAuth.expiresAt,
      refreshExpiresAt: nextAuth.refreshExpiresAt,
    });
    log('[Accounts]', `✅ 账号 token 已刷新: ${creds.name}（${creds.uid.slice(0, 8)}…）`);
  }

  function errorStatus(error) {
    if (error instanceof AccountStoreError) return error.statusCode || 400;
    if (error instanceof ProxyConfigError) return error.statusCode || 400;
    if (error instanceof WorkBuddyAuthError) {
      return Number.isInteger(error.statusCode) ? error.statusCode : 502;
    }
    return 500;
  }

  /** 目标账号集合：给 id 就只取该账号，否则取全部启用账号 */
  function resolveTargets(id) {
    const snapshot = store.listAccounts();
    if (typeof id === 'string' && id) {
      const found = snapshot.accounts.filter(account => account.id === id);
      if (!found.length) throw new AccountStoreError('账号不存在', 404);
      return found;
    }
    return snapshot.accounts.filter(account => account.available !== false);
  }

  /** 批量操作（积分/签到）的目标：默认跳过已禁用账号，避免无谓地打上游 */
  function resolveBatchTargets(id) {
    const targets = resolveTargets(id);
    if (typeof id === 'string' && id) return { targets, skipped: 0 };
    const enabled = targets.filter(account => account.enabled !== false);
    return { targets: enabled, skipped: targets.length - enabled.length };
  }

  /** 国际版没有签到活动，签到相关操作一律排除该版本账号 */
  function supportsCheckin(account) {
    return account.edition !== 'intl';
  }

  /**
   * 签到目标集合：批量时只取国内版账号（国际版无此功能，直接跳过而不是报错）；
   * 指定单个账号时若为国际版则明确报错，避免用户误以为签到失败是网络问题。
   */
  function resolveCheckinTargets(id) {
    const targets = resolveTargets(id);
    if (typeof id === 'string' && id) {
      if (!supportsCheckin(targets[0])) {
        throw new AccountStoreError('国际版账号暂不支持签到', 400);
      }
      return { targets, skipped: 0 };
    }
    const eligible = targets.filter(account => account.enabled !== false).filter(supportsCheckin);
    return { targets: eligible, skipped: targets.length - eligible.length };
  }

  /**
   * 单个账号的积分查询。token 失效时刷新后重试一次；
   * 任何失败都收敛为 { error } 而不是抛出，保证批量查询不被单个账号拖垮。
   */
  async function queryUsageFor(account) {
    try {
      let entry = store.getSessionById(account.id);
      if (!entry) return { id: account.id, usage: null, error: '没有可用凭证' };
      try {
        const usage = await billing.queryCreditsSummary({ session: entry.session });
        return { id: account.id, name: account.name, usage, error: null };
      } catch (error) {
        // 401 说明 token 被服务端拒绝而非临期：刷新后重试一次
        if (error?.statusCode !== 401) throw error;
        const creds = store.getCredentialsById(account.id);
        if (!creds?.refreshToken) throw error;
        await refreshById(account.id);
        entry = store.getSessionById(account.id);
        if (!entry) throw error;
        const usage = await billing.queryCreditsSummary({ session: entry.session });
        return { id: account.id, name: account.name, usage, error: null };
      }
    } catch (error) {
      verbose('[Accounts]', `账号 ${account.id} 积分查询失败: ${error.message}`);
      return {
        id: account.id,
        name: account.name,
        usage: null,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /** 单个账号签到。已签到（非 0 code）不算错误，原样返回结果 */
  async function checkinFor(account) {
    try {
      const entry = store.getSessionById(account.id);
      if (!entry) return { id: account.id, name: account.name, claim: null, error: '没有可用凭证' };
      const claim = await billing.claimDailyCheckin({ session: entry.session });
      log('[Accounts]',
        `账号 ${account.name}: 签到${claim.success ? '成功' : `未领取（${claim.msg}）`}`);
      return { id: account.id, name: account.name, claim, error: null };
    } catch (error) {
      verbose('[Accounts]', `账号 ${account.id} 签到失败: ${error.message}`);
      return {
        id: account.id,
        name: account.name,
        claim: null,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  async function tryHandle(req, res, path) {
    if (path !== '/api/accounts' && !path.startsWith('/api/accounts/')) return false;
    if (!checkApiKey(req)) { unauthorized(res); return true; }

    try {
      // 账号列表
      if (req.method === 'GET' && path === '/api/accounts') {
        sendJson(res, 200, { success: true, data: store.listAccounts() });
        return true;
      }

      // 批量查询各账号积分（并发，单账号失败不影响其他；默认只查启用中的账号）
      if (req.method === 'GET' && path === '/api/accounts/usage') {
        if (!billing) throw new AccountStoreError('积分模块未启用', 503);
        const { targets, skipped } = resolveBatchTargets(null);
        const results = await Promise.all(targets.map(queryUsageFor));
        sendJson(res, 200, { success: true, data: { results, skipped } });
        return true;
      }

      // 导出全部账号（含 accessToken / refreshToken，换机器后导入即可继续用）
      // 注意：必须放在下面的 /api/accounts/<id> 系列通配路由之前，
      // 否则 export 会被当成账号 id 处理
      if (req.method === 'GET' && path === '/api/accounts/export') {
        const data = store.exportAccounts();
        log('[Accounts]', `📤 账号已导出: ${data.accounts.length} 个`);
        sendJson(res, 200, { success: true, data });
        return true;
      }

      // 手动添加账号
      if (req.method === 'POST' && path === '/api/accounts') {
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let payload;
        try {
          payload = JSON.parse(raw);
        } catch {
          throw new AccountStoreError('上传内容不是有效 JSON');
        }
        const account = store.addAccount(payload);
        sendJson(res, 200, { success: true, data: { account, list: store.listAccounts() } });
        return true;
      }

      // 导入账号（导出文件的回流，merge 语义：按 uid 匹配，命中更新、未命中追加）
      // 同样放在 /api/accounts/<id> 系列通配路由之前，避免 import 被当成账号 id
      if (req.method === 'POST' && path === '/api/accounts/import') {
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let payload;
        try {
          payload = JSON.parse(raw || '{}');
        } catch {
          throw new AccountStoreError('请求内容不是有效 JSON');
        }
        const result = store.importAccounts(payload);
        sendJson(res, 200, { success: true, data: result });
        return true;
      }

      // 切换当前账号 = 把该账号置顶（当前账号由优先级派生，所以置顶即切换）
      if (req.method === 'POST' && path === '/api/accounts/current') {
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        const { id } = JSON.parse(raw || '{}');
        if (typeof id !== 'string' || !id) throw new AccountStoreError('缺少账号 id');
        const result = store.promoteToFront(id);
        if (!result.changed) log('[Accounts]', `已是当前账号，无需切换: ${id}`);
        sendJson(res, 200, { success: true, data: { ...result, list: store.listAccounts() } });
        return true;
      }

      // 批量操作：启用 / 禁用 / 改代理 / 删除
      if (req.method === 'POST' && path === '/api/accounts/batch') {
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let payload;
        try {
          payload = JSON.parse(raw || '{}');
        } catch {
          throw new AccountStoreError('请求内容不是有效 JSON');
        }
        const ids = Array.isArray(payload?.ids)
          ? [...new Set(payload.ids.filter(id => typeof id === 'string' && id))]
          : [];
        if (!ids.length) throw new AccountStoreError('缺少要操作的账号 id');
        const action = String(payload?.action || '').trim();

        if (action === 'remove') {
          const result = store.batchRemove(ids);
          sendJson(res, 200, { success: true, data: { action, ...result, list: store.listAccounts() } });
          return true;
        }
        if (action === 'enable' || action === 'disable') {
          const result = store.batchUpdate(ids, { enabled: action === 'enable' });
          sendJson(res, 200, { success: true, data: { action, ...result, list: store.listAccounts() } });
          return true;
        }
        if (action === 'proxy') {
          // 允许 proxy=null（批量改为直连）
          if (!Object.prototype.hasOwnProperty.call(payload, 'proxy')) {
            throw new AccountStoreError('批量修改代理时缺少 proxy 字段');
          }
          const result = store.batchUpdate(ids, { proxy: payload.proxy });
          sendJson(res, 200, { success: true, data: { action, ...result, list: store.listAccounts() } });
          return true;
        }
        throw new AccountStoreError(`不支持的批量操作: ${action || '(空)'}`);
      }

      // 刷新指定（缺省当前）账号 token
      if (req.method === 'POST' && path === '/api/accounts/refresh') {
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        const parsed = JSON.parse(raw || '{}');
        let id = typeof parsed?.id === 'string' ? parsed.id : null;
        if (!id) {
          const entry = store.getActiveEntry();
          if (!entry) throw new AccountStoreError('当前没有可用账号');
          id = entry.id;
        }
        verbose('[Accounts]', `刷新账号 token: ${id}`);
        await refreshById(id);
        sendJson(res, 200, { success: true, data: { refreshedId: id, list: store.listAccounts() } });
        return true;
      }

      // 批量/单个签到（国际版账号无签到活动，不参与；批量时跳过已禁用账号）
      if (req.method === 'POST' && path === '/api/accounts/checkin') {
        if (!billing) throw new AccountStoreError('积分模块未启用', 503);
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        const parsed = JSON.parse(raw || '{}');
        const id = typeof parsed?.id === 'string' ? parsed.id : null;
        const { targets, skipped } = resolveCheckinTargets(id);
        // 串行签到：避免多账号同时打上游触发 11128 风控
        const results = [];
        for (const account of targets) results.push(await checkinFor(account));
        const succeeded = results.filter(r => r.claim?.success).length;
        if (skipped) log('[Accounts]', `已跳过 ${skipped} 个账号（已禁用或国际版无签到活动）`);
        log('[Accounts]', `签到完成: ${succeeded}/${results.length} 个账号成功领取`);
        sendJson(res, 200, { success: true, data: { results, succeeded, total: results.length, skipped } });
        return true;
      }

      // 修改账号属性（优先级 / 启用状态 / 备注名 / 代理）
      if (req.method === 'PATCH' && path.startsWith('/api/accounts/')) {
        const id = decodeURIComponent(path.slice('/api/accounts/'.length));
        if (!id) throw new AccountStoreError('缺少账号 id');
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let patch;
        try {
          patch = JSON.parse(raw || '{}');
        } catch {
          throw new AccountStoreError('请求内容不是有效 JSON');
        }
        if (!patch || typeof patch !== 'object' || Array.isArray(patch)) {
          throw new AccountStoreError('请求内容必须是 JSON 对象');
        }
        const result = store.updateAccount(id, patch);
        sendJson(res, 200, {
          success: true,
          data: { account: result.account, changes: result.changes, list: store.listAccounts() },
        });
        return true;
      }

      // 调整转发顺序：与相邻账号交换优先级（优先级唯一，靠交换重排最直观）
      if (req.method === 'POST' && path.startsWith('/api/accounts/') && path.endsWith('/move')) {
        const id = decodeURIComponent(path.slice('/api/accounts/'.length, -'/move'.length));
        if (!id) throw new AccountStoreError('缺少账号 id');
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let direction = 'up';
        try {
          direction = JSON.parse(raw || '{}')?.direction === 'down' ? 'down' : 'up';
        } catch {
          throw new AccountStoreError('请求内容不是有效 JSON');
        }
        const result = store.moveAccount(id, direction);
        if (!result.moved) log('[Accounts]', `顺序未变（${result.reason}）`);
        sendJson(res, 200, { success: true, data: result.moved ? result : { ...result, list: store.listAccounts() } });
        return true;
      }

      // 删除账号
      if (req.method === 'DELETE' && path.startsWith('/api/accounts/')) {
        const id = decodeURIComponent(path.slice('/api/accounts/'.length));
        if (!id) throw new AccountStoreError('缺少账号 id');
        store.removeAccount(id);
        sendJson(res, 200, { success: true, data: { list: store.listAccounts() } });
        return true;
      }

      sendJson(res, 404, { success: false, error: `Not found: ${req.method} ${path}` });
      return true;
    } catch (error) {
      log('[Accounts]', `❌ ${error.message}`);
      sendJson(res, errorStatus(error), { success: false, error: error.message });
      return true;
    }
  }

  /**
   * 出网代理相关路由：
   *   GET  /api/proxies       可选项（Clash Verge 实时快照）+ 各账号当前出口
   *   POST /api/proxies/test  测试出口连通性并返回出口 IP
   */
  async function tryHandleProxies(req, res, path) {
    if (path !== '/api/proxies' && path !== '/api/proxies/test') return false;
    if (!checkApiKey(req)) { unauthorized(res); return true; }

    try {
      if (req.method === 'GET' && path === '/api/proxies') {
        const clash = clashProxyOptions();
        const accounts = store.listAccounts().accounts.map(account => ({
          id: account.id,
          name: account.name,
          priority: account.priority,
          enabled: account.enabled,
          proxy: account.proxy,
        }));
        sendJson(res, 200, {
          success: true,
          data: {
            clash: { available: clash.available, error: clash.error, dir: clash.dir, options: clash.options },
            accounts,
            note: '账号默认无代理（直连）；选择 Clash 项后端口由 Clash Verge 管理，这里实时读取',
          },
        });
        return true;
      }

      if (req.method === 'POST' && path === '/api/proxies/test') {
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let payload;
        try {
          payload = JSON.parse(raw || '{}');
        } catch {
          throw new ProxyConfigError('请求内容不是有效 JSON');
        }
        // 两种用法：{ id } 测某账号的出口；{ proxy } 直接测临时配置（含 null = 测直连）
        let proxy = null;
        if (typeof payload?.id === 'string' && payload.id) {
          const creds = store.getCredentialsById(payload.id);
          if (!creds) throw new AccountStoreError('账号不存在', 404);
          if (creds.proxyError) throw new ProxyConfigError(creds.proxyError);
          proxy = creds.proxy;
        } else if (payload && Object.prototype.hasOwnProperty.call(payload, 'proxy')) {
          const config = normalizeAccountProxy(payload.proxy);
          const resolved = config ? resolveAccountProxy(config) : null;
          if (resolved?.error) throw new ProxyConfigError(resolved.error);
          proxy = resolved;
        }
        const result = await testProxyConnectivity(proxy);
        log('[Accounts]',
          `代理测试${proxy ? `（${proxy.label || proxy.host}）` : '（直连）'}: ${result.success ? `✅ 出口 IP ${result.ip}` : `❌ ${result.error}`}`);
        sendJson(res, 200, { success: true, data: { ...result, proxy: proxy ? { label: proxy.label || '', host: proxy.host, port: proxy.port } : null } });
        return true;
      }

      sendJson(res, 404, { success: false, error: `Not found: ${req.method} ${path}` });
      return true;
    } catch (error) {
      log('[Accounts]', `❌ ${error.message}`);
      sendJson(res, errorStatus(error), { success: false, error: error.message });
      return true;
    }
  }

  return { tryHandle, tryHandleProxies };
}
