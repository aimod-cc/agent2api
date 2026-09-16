/**
 * WorkBuddy 运行日志 — 查询路由
 *
 *   GET    /api/logs                 查询日志（?limit=&level=&category=&keyword=）
 *   GET    /api/logs/stats           各级别 / 分类计数（导航徽标、筛选下拉）
 *   GET    /api/logs/download        导出 JSONL 附件
 *   DELETE /api/logs                 清空日志
 *
 * tryHandle 返回 true 表示请求已被处理（与脱敏路由同一约定）。
 */

import { CATEGORIES, LEVELS, MAX_ENTRIES } from './workbuddy-logs.mjs';

function sendJson(res, status, payload) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(payload));
}

export function setupLogRoutes({
  logStore,
  checkApiKey = () => true,
  unauthorized = () => {},
  log = () => {},
}) {
  async function tryHandle(req, res, path, url) {
    if (path !== '/api/logs' && !path.startsWith('/api/logs/')) return false;
    if (!checkApiKey(req)) { unauthorized(res); return true; }

    try {
      if (!logStore) {
        sendJson(res, 503, { success: false, error: '日志模块未启用' });
        return true;
      }

      const action = path.slice('/api/logs'.length).replace(/^\/+/, '');

      // 计数（导航徽标 / 筛选下拉的选项与数量）
      if (req.method === 'GET' && action === 'stats') {
        sendJson(res, 200, {
          success: true,
          data: { ...logStore.stats(), levels: LEVELS, categories: CATEGORIES, max: MAX_ENTRIES },
        });
        return true;
      }

      // 导出：直接回 JSONL 附件，浏览器/桌面端存盘即可
      if (req.method === 'GET' && action === 'download') {
        const body = logStore.toJsonl();
        const stamp = new Date().toISOString().replace(/[:.]/g, '-');
        res.writeHead(200, {
          'Content-Type': 'application/x-ndjson; charset=utf-8',
          'Content-Disposition': `attachment; filename="workbuddy-logs-${stamp}.jsonl"`,
          'Content-Length': Buffer.byteLength(body),
        });
        res.end(body);
        return true;
      }

      // 查询
      if (req.method === 'GET' && !action) {
        const data = logStore.query({
          limit: url?.searchParams?.get('limit'),
          level: url?.searchParams?.get('level'),
          category: url?.searchParams?.get('category'),
          keyword: url?.searchParams?.get('keyword'),
          sinceId: url?.searchParams?.get('sinceId'),
        });
        sendJson(res, 200, {
          success: true,
          data: { ...data, levels: LEVELS, categories: CATEGORIES, max: MAX_ENTRIES },
        });
        return true;
      }

      // 清空
      if (req.method === 'DELETE' && !action) {
        const stats = logStore.clear();
        log('[Logs]', '运行日志已清空');
        sendJson(res, 200, { success: true, data: stats });
        return true;
      }

      sendJson(res, 404, { success: false, error: `Not found: ${req.method} ${path}` });
      return true;
    } catch (error) {
      const status = Number.isInteger(error?.statusCode) ? error.statusCode : 500;
      log('[Logs]', `❌ ${error.message}`);
      sendJson(res, status, { success: false, error: error.message });
      return true;
    }
  }

  return { tryHandle };
}
