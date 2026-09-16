/**
 * WorkBuddy 敏感词脱敏 — 词表维护路由
 *
 *   GET    /api/desensitize              当前状态（开关 / 词表 / 角色 / 命中统计）
 *   POST   /api/desensitize/enabled      开关 { enabled }
 *   POST   /api/desensitize/roles        作用角色 { roles: ['system','user'] }
 *   PUT    /api/desensitize/terms        全量替换词表 { terms: [...] }
 *   POST   /api/desensitize/terms        追加词 { terms: [...] | term: '...' }
 *   DELETE /api/desensitize/terms        删除词 { terms: [...] } 或 ?term=xxx
 *   POST   /api/desensitize/reset        恢复默认词表
 *   POST   /api/desensitize/stats/reset  清空命中统计
 *
 * tryHandle 返回 true 表示请求已被处理。
 */

import { MAX_TERMS, MAX_TERM_LENGTH } from './workbuddy-desensitize.mjs';

function sendJson(res, status, payload) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(payload));
}

/** 从请求体里取词：兼容 { terms: [...] }、{ term: '...' } 与裸数组 */
function extractTerms(body, url) {
  if (Array.isArray(body)) return body;
  if (Array.isArray(body?.terms)) return body.terms;
  if (typeof body?.term === 'string') return [body.term];
  const fromQuery = url?.searchParams?.get('term');
  if (fromQuery) return [fromQuery];
  return null;
}

async function readJson(req, readRawBody) {
  const raw = (await readRawBody(req)).toString('utf8') || '{}';
  try {
    return JSON.parse(raw);
  } catch {
    const error = new Error('请求体不是有效 JSON');
    error.statusCode = 400;
    throw error;
  }
}

function validateTerms(terms) {
  if (!Array.isArray(terms)) {
    const error = new Error('缺少词表：请提供 { terms: [...] } 或 { term: "..." }');
    error.statusCode = 400;
    throw error;
  }
  if (!terms.length) {
    const error = new Error('词表为空：至少需要一个词');
    error.statusCode = 400;
    throw error;
  }
  const invalid = terms.find(item => typeof item !== 'string' || !item.trim());
  if (invalid !== undefined) {
    const error = new Error('词表只接受非空字符串');
    error.statusCode = 400;
    throw error;
  }
  const tooLong = terms.find(item => item.trim().length > MAX_TERM_LENGTH);
  if (tooLong !== undefined) {
    const error = new Error(`单个词长度不能超过 ${MAX_TERM_LENGTH} 个字符`);
    error.statusCode = 400;
    throw error;
  }
  if (terms.length > MAX_TERMS) {
    const error = new Error(`词表最多 ${MAX_TERMS} 个词`);
    error.statusCode = 400;
    throw error;
  }
  return terms;
}

export function setupDesensitizeRoutes({
  desensitizer,
  checkApiKey = () => true,
  unauthorized = () => {},
  readRawBody,
  log = () => {},
}) {
  async function tryHandle(req, res, path, url) {
    if (path !== '/api/desensitize' && !path.startsWith('/api/desensitize/')) return false;
    if (!checkApiKey(req)) { unauthorized(res); return true; }

    try {
      if (!desensitizer) {
        sendJson(res, 503, { success: false, error: '脱敏模块未启用' });
        return true;
      }

      const action = path.slice('/api/desensitize'.length).replace(/^\/+/, '');

      // 当前状态
      if (req.method === 'GET' && !action) {
        sendJson(res, 200, { success: true, data: desensitizer.getState() });
        return true;
      }

      // 开关
      if (req.method === 'POST' && action === 'enabled') {
        const body = await readJson(req, readRawBody);
        if (typeof body?.enabled !== 'boolean') {
          sendJson(res, 400, { success: false, error: '缺少 enabled（需要布尔值）' });
          return true;
        }
        const state = desensitizer.setEnabled(body.enabled);
        log('[Desensitize]', `内容脱敏已${body.enabled ? '开启' : '关闭'}`);
        sendJson(res, 200, { success: true, data: state });
        return true;
      }

      // 作用角色
      if (req.method === 'POST' && action === 'roles') {
        const body = await readJson(req, readRawBody);
        const state = desensitizer.setRoles(body?.roles);
        log('[Desensitize]', `作用角色: ${state.roles.join('、')}`);
        sendJson(res, 200, { success: true, data: state });
        return true;
      }

      // 词表：全量替换 / 追加
      if ((req.method === 'PUT' || req.method === 'POST') && action === 'terms') {
        const body = await readJson(req, readRawBody);
        const terms = validateTerms(extractTerms(body, url));
        const before = desensitizer.terms.length;
        const state = req.method === 'PUT'
          ? desensitizer.setTerms(terms)
          : desensitizer.addTerms(terms);
        log('[Desensitize]', `词表${req.method === 'PUT' ? '更新' : '追加'}: ${before} → ${state.termCount} 个词`);
        sendJson(res, 200, { success: true, data: state });
        return true;
      }

      // 删词
      if (req.method === 'DELETE' && action === 'terms') {
        const body = await readJson(req, readRawBody).catch(() => ({}));
        const terms = validateTerms(extractTerms(body, url));
        const state = desensitizer.removeTerms(terms);
        log('[Desensitize]', `删除 ${terms.length} 个词，剩余 ${state.termCount} 个`);
        sendJson(res, 200, { success: true, data: state });
        return true;
      }

      // 恢复默认词表
      if (req.method === 'POST' && action === 'reset') {
        const state = desensitizer.resetTerms();
        log('[Desensitize]', `词表已恢复默认（${state.termCount} 个词）`);
        sendJson(res, 200, { success: true, data: state });
        return true;
      }

      // 清空命中统计
      if (req.method === 'POST' && action === 'stats/reset') {
        desensitizer.resetStats();
        sendJson(res, 200, { success: true, data: desensitizer.getState() });
        return true;
      }

      sendJson(res, 404, { success: false, error: `Not found: ${req.method} ${path}` });
      return true;
    } catch (error) {
      const status = Number.isInteger(error?.statusCode) ? error.statusCode : 500;
      log('[Desensitize]', `❌ ${error.message}`);
      sendJson(res, status, { success: false, error: error.message });
      return true;
    }
  }

  return { tryHandle };
}
