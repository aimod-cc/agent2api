/**
 * 启动横幅（终端里打印的接口清单）。
 *
 * 纯展示文本，与运行时逻辑无关，单独成文件以便 server.mjs 保持精简。
 * 新增接口时记得同步这里，方便用户一眼看到可用端点。
 */

const LINE = '  ┌────────────────────────────────────────────────────────────┐';
const END = '  └────────────────────────────────────────────────────────────┘';
const GAP = '  ├────────────────────────────────────────────────────────────┤';
const WIDTH = 58;

/**
 * 显示宽度：中日韩字形占 2 列，其余占 1 列。
 * 直接用 String.length 会把中文行算窄，导致右边框错位。
 */
function displayWidth(text) {
  let width = 0;
  for (const ch of String(text)) {
    const code = ch.codePointAt(0);
    const wide = (code >= 0x1100 && code <= 0x115f)        // 韩文字母
      || (code >= 0x2e80 && code <= 0xa4cf)                // CJK 部首 / 假名 / 汉字
      || (code >= 0xac00 && code <= 0xd7a3)                // 韩文音节
      || (code >= 0xf900 && code <= 0xfaff)                // CJK 兼容表意
      || (code >= 0xfe30 && code <= 0xfe6f)                // CJK 兼容形式
      || (code >= 0xff00 && code <= 0xff60)                // 全角字符
      || (code >= 0xffe0 && code <= 0xffe6);
    width += wide ? 2 : 1;
  }
  return width;
}

/** 把一行内容对齐到横幅宽度内（超出则截断，避免边框错位） */
function row(text) {
  const raw = String(text);
  let content = raw;
  while (displayWidth(content) > WIDTH) content = content.slice(0, -1);
  return `  │ ${content}${' '.repeat(WIDTH - displayWidth(content))} │`;
}

export function printStartupBanner(host, port, { apiKey } = {}) {
  console.log('');
  console.log(LINE);
  for (const line of [
    `API 服务: http://${host}:${port}`,
    `API Key 认证: ${apiKey ? '已启用' : '未启用（仅本机可访问）'}`,
  ]) console.log(row(line));
  console.log(GAP);
  console.log(row('模型端点:'));
  for (const line of [
    'POST   /v1/chat/completions    (OpenAI 兼容，SSE 透传)',
    'POST   /v1/completions         (代码补全)',
    'POST   /v1/embeddings          (向量)',
    'POST   /v1/images/generations  (文生图)',
    'GET    /v1/models              (模型列表)',
  ]) console.log(row(line));
  console.log(GAP);
  console.log(row('积分 / 签到:'));
  for (const line of [
    'GET    /api/usage              (积分/额度查询)',
    'GET    /api/checkin/status     (签到活动状态)',
    'POST   /api/checkin            (领取每日签到积分)',
    'POST   /api/checkin/claim-and-report (签到+回报积分)',
    'GET    /api/activity/banner    (运营横幅)',
    'GET    /api/activity/ambassador(大使状态)',
  ]) console.log(row(line));
  console.log(GAP);
  console.log(row('账号 / 路由:'));
  for (const line of [
    'GET    /api/accounts           (账号列表)',
    'PATCH  /api/accounts/<id>      (优先级/启用/代理/备注)',
    'GET    /api/proxies            (出网代理可选项)',
    'POST   /api/proxies/test       (出口连通性测试)',
  ]) console.log(row(line));
  console.log(GAP);
  console.log(row('管理:'));
  for (const line of [
    'GET    /health                 (健康检查)',
    'GET    /api/session            (会话总览)',
    'GET    /api/endpoints          (已逆向接口清单)',
    'GET    /api/desensitize        (敏感词脱敏状态/词表)',
    'POST   /auth/login|logout      (登录态管理)',
  ]) console.log(row(line));
  console.log(END);
  console.log('');
}
