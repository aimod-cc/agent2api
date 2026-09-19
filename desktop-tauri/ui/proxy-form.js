/* Agent2API · 出网代理表单（可复用组件） */
/* global workbuddyDesktop, wbApp */

/**
 * 代理表单组件：把「无代理 / Clash Verge / 自定义」三选一的表单封装成可复用实例，
 * 供账号设置弹窗与批量修改代理弹窗共用 —— 两处的校验、读取、测试逻辑完全一致，
 * 避免各写一份导致行为分叉。
 *
 * 用法：
 *   const form = wbProxyForm.create(document.getElementById('account-proxy-form'));
 *   await form.loadClashOptions();          // 拉取 Clash 出口列表（带缓存）
 *   form.fill(account.proxy);               // 按账号已有配置回填
 *   const proxy = form.read();              // 读取为接口 payload（非法时抛错）
 *
 * 组件自己渲染标记（而不是复用页面上写死的 id）：同页面可能有多个实例，
 * 共用 id / radio name 会互相串台，因此每次实例化都用独立命名空间。
 *
 * 代理配置形态（与后端 workbuddy-proxy.mjs 一致）：
 *   null                                              → 无代理（直连）
 *   { source: 'clash',  listenerUid }                 → 走 Clash Verge 监听器
 *   { source: 'custom', protocol, host, port, ... }   → 自定义 http/socks5
 */
(() => {
  const api = workbuddyDesktop;
  const { esc, toast } = wbApp;

  /** Clash 出口列表缓存：一次页面加载内多实例共享，避免重复请求 */
  let clashCache = null;
  let instanceSeq = 0;

  /** 账号 proxy 字段 → 表单模式 */
  function modeOf(proxy) {
    const source = proxy?.config?.source || proxy?.source;
    if (source === 'clash') return 'clash';
    if (source === 'custom') return 'custom';
    return 'none';
  }

  function create(container) {
    if (!container) throw new Error('代理表单需要挂载容器');
    const uid = `pf${++instanceSeq}`;
    const modeName = `proxy-mode-${uid}`;

    container.innerHTML = `
      <div class="field-row">
        <label class="edition-option"><input type="radio" name="${modeName}" value="none" checked><span>无代理（直连）</span></label>
        <label class="edition-option"><input type="radio" name="${modeName}" value="clash"><span>Clash Verge</span></label>
        <label class="edition-option"><input type="radio" name="${modeName}" value="custom"><span>自定义</span></label>
      </div>

      <div class="pf-clash" style="display:none;margin-top:12px">
        <div class="field-row">
          <label>出口</label>
          <select class="proxy-select pf-clash-select"></select>
          <button type="button" class="pf-clash-refresh">重新读取</button>
        </div>
        <div class="detail pf-clash-hint" style="margin-top:6px"></div>
      </div>

      <div class="pf-custom" style="display:none;margin-top:12px">
        <div class="field-row">
          <label>协议</label>
          <select class="proxy-select pf-protocol">
            <option value="http">HTTP</option>
            <option value="socks5">SOCKS5</option>
          </select>
          <label style="margin-left:10px">主机</label>
          <input type="text" class="pf-host" placeholder="127.0.0.1">
          <label>端口</label>
          <input type="number" class="pf-port" min="1" max="65535" style="max-width:110px" placeholder="7890">
        </div>
        <div class="field-row" style="margin-top:10px">
          <label>用户名</label>
          <input type="text" class="pf-username" placeholder="可选" autocomplete="off">
          <label>密码</label>
          <input type="password" class="pf-password" placeholder="可选" autocomplete="new-password">
        </div>
      </div>

      <div class="field-row" style="margin-top:12px">
        <button type="button" class="pf-test">测试出口</button>
        <span class="detail pf-test-result"></span>
      </div>
    `;

    const $ = cls => container.querySelector(`.${cls}`);
    const radios = [...container.querySelectorAll(`input[name="${modeName}"]`)];
    let busy = false;

    function setMode(mode) {
      radios.forEach(input => { input.checked = input.value === mode; });
      $('pf-clash').style.display = mode === 'clash' ? '' : 'none';
      $('pf-custom').style.display = mode === 'custom' ? '' : 'none';
    }

    function currentMode() {
      return radios.find(input => input.checked)?.value || 'none';
    }

    /** 填充 Clash 出口下拉；selectedUid 不存在时回落到第一项 */
    function fillClashOptions(clash, selectedUid) {
      const select = $('pf-clash-select');
      const hint = $('pf-clash-hint');
      const options = Array.isArray(clash?.options) ? clash.options : [];

      if (!clash?.available) {
        select.innerHTML = '<option value="">未检测到 Clash Verge</option>';
        select.disabled = true;
        hint.textContent = clash?.error ? `不可用：${clash.error}` : '未检测到 Clash Verge 配置';
        return;
      }
      if (!options.length) {
        select.innerHTML = '<option value="">没有可用出口</option>';
        select.disabled = true;
        hint.textContent = 'Clash Verge 里还没有配置混合监听器或节点端口';
        return;
      }
      select.disabled = false;
      select.innerHTML = options.map(item => {
        const inactive = item.profileActive === false ? '（其他订阅，可能未生效）' : '';
        const disabled = item.enabled === false ? '（已在 Clash 中禁用）' : '';
        return `<option value="${esc(item.uid)}">${esc(item.name)} :${esc(item.port)}${inactive}${disabled}</option>`;
      }).join('');
      if (selectedUid && options.some(item => item.uid === selectedUid)) select.value = selectedUid;
      hint.textContent = `读取自 ${clash.dir || 'Clash Verge'}；端口由 Clash Verge 管理，这里实时同步`;
    }

    /** 拉取 Clash 出口（默认走缓存；force=true 重新读取） */
    async function loadClashOptions({ force = false, selectedUid = null } = {}) {
      try {
        if (!clashCache || force) clashCache = await api.getProxies();
        fillClashOptions(clashCache?.clash, selectedUid);
        return clashCache;
      } catch (error) {
        fillClashOptions({ available: false, error: error.message });
        return null;
      }
    }

    /** 按账号已有的代理配置回填表单 */
    function fill(proxy) {
      const mode = modeOf(proxy);
      setMode(mode);
      const config = proxy?.config || null;
      if (mode === 'custom' && config) {
        $('pf-protocol').value = config.protocol === 'socks5' ? 'socks5' : 'http';
        $('pf-host').value = config.host || '';
        $('pf-port').value = config.port ?? '';
        $('pf-username').value = config.username || '';
        $('pf-password').value = config.password || '';
      } else {
        $('pf-host').value = '';
        $('pf-port').value = '';
        $('pf-username').value = '';
        $('pf-password').value = '';
      }
      $('pf-test-result').textContent = '';
      return mode;
    }

    /** 读取表单为接口 payload；非法输入抛出「面向用户」的错误 */
    function read() {
      const mode = currentMode();
      if (mode === 'none') return null;
      if (mode === 'clash') {
        const uidValue = $('pf-clash-select').value;
        if (!uidValue) throw new Error('请先选择 Clash Verge 出口');
        return { source: 'clash', listenerUid: uidValue };
      }
      const protocol = $('pf-protocol').value === 'socks5' ? 'socks5' : 'http';
      const host = $('pf-host').value.trim();
      const port = Number($('pf-port').value);
      if (!host) throw new Error('请填写代理主机地址');
      if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('代理端口必须是 1-65535 的整数');
      return {
        source: 'custom',
        protocol,
        host,
        port,
        username: $('pf-username').value.trim(),
        password: $('pf-password').value,
      };
    }

    /** 用当前表单内容测试出口连通性 */
    async function test() {
      if (busy) return;
      let proxy;
      try {
        proxy = read();
      } catch (error) {
        toast(error.message, 'err');
        return;
      }
      busy = true;
      const button = $('pf-test');
      const result = $('pf-test-result');
      button.disabled = true;
      button.textContent = '测试中…';
      result.textContent = '正在连接上游…';
      try {
        const data = await api.testProxy({ proxy });
        if (data?.success) {
          result.innerHTML = '<span style="color:var(--ok)">✅ 出口可用</span>'
            + `　HTTP ${esc(String(data.status ?? ''))}`
            + (data.ip ? `　出口 IP ${esc(data.ip)}` : '')
            + `　${esc(String(data.durationMs ?? ''))}ms`;
        } else {
          result.innerHTML = `<span style="color:var(--danger)">❌ ${esc(data?.error || '连接失败')}</span>`;
        }
      } catch (error) {
        result.innerHTML = `<span style="color:var(--danger)">❌ ${esc(error.message)}</span>`;
      } finally {
        busy = false;
        button.disabled = false;
        button.textContent = '测试出口';
      }
    }

    radios.forEach(input => input.addEventListener('change', () => setMode(input.value)));
    $('pf-clash-refresh').addEventListener('click', async () => {
      const data = await loadClashOptions({ force: true, selectedUid: $('pf-clash-select').value });
      if (data) toast('✅ 已重新读取 Clash Verge 配置');
    });
    $('pf-test').addEventListener('click', test);

    return {
      fill,
      read,
      test,
      setMode,
      currentMode,
      loadClashOptions,
      invalidateClashCache: () => { clashCache = null; },
      setBusy: value => { busy = value === true; },
    };
  }

  window.wbProxyForm = { create, modeOf, invalidateClashCache: () => { clashCache = null; } };
})();
