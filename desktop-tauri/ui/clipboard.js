/* 通用剪贴板工具：给页面上带 data-copy / data-copy-from 的按钮用（网关页的复制按钮、弹窗里的复制按钮都走它） */

// ─── 复制 ─────────────────────────────────────

/**
 * 复制按钮统一入口。两种用法：
 *   data-copy="文本"          直接复制给定文本（模型芯片）
 *   data-copy-from="元素 id"  复制该元素的当前文本（地址 / UID 等会变的值）
 * 地址类文本带 "POST " / "GET " 前缀，复制时去掉，保证粘出去能直接用。
 */
function copyTextOf(trigger) {
  const fromId = trigger.dataset.copyFrom;
  if (fromId) {
    const source = $(fromId);
    if (!source) return '';
    return source.textContent.replace(/^(POST|GET)\s+/i, '').trim();
  }
  return trigger.dataset.copy || '';
}

async function copyToClipboard(text) {
  if (!text) return false;
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // 剪贴板被拒（无权限 / 非安全上下文）时用兜底方案，避免功能静默失效
    try {
      const area = document.createElement('textarea');
      area.value = text;
      area.style.position = 'fixed';
      area.style.opacity = '0';
      document.body.appendChild(area);
      area.select();
      const ok = document.execCommand('copy');
      area.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

document.addEventListener('click', async event => {
  const trigger = event.target.closest('[data-copy], [data-copy-from]');
  if (!trigger) return;
  const text = copyTextOf(trigger);
  if (!(await copyToClipboard(text))) { toast('复制失败，请手动选择复制', 'err'); return; }
  toast(`已复制：${text.length > 46 ? `${text.slice(0, 46)}…` : text}`);
  // 复制按钮给个即时反馈（模型芯片本身是内容，不改它的外观）
  if (trigger.classList.contains('copy-btn')) {
    trigger.classList.add('done');
    trigger.textContent = '✓';
    setTimeout(() => {
      trigger.classList.remove('done');
      trigger.textContent = '⧉';
    }, 1200);
  }
});
