/* Agent2API · ZCode「周末套餐」的探测与领取（账号页那一列按钮）

   ── 这一家为什么没有「签到」而是这个 ────────────────────────
   其余各家的运营玩法是每日签到（见 core/auto_checkin），ZCode 是**限时发放的
   体验套餐**（官方叫 start-plan / 周末套餐，在客户端里点一下就领）。因此本家
   在账号行上给的是「领套餐」而不是「签到」。

   ── 两步，而且只有第二步要验证码 ────────────────────────────
     ① 探测  POST /api/accounts/{id}/zcode-claim/preview   —— 只读，不要验证码
     ② 领取  POST /api/accounts/{id}/zcode-claim           —— **要**验证码

   上游的领取接口强制要求 `X-Aliyun-Captcha-Verify-Param`（阿里云无痕验证），
   而解它的唯一可行方式是在 webview 里跑阿里云官方 SDK —— 那正是
   `ui/aliyun-captcha.js` 提供的东西（同一个求解器也给 AutoClaw 的 OAuth 用）。

   ── 为什么探测值得单独跑一次 ────────────────────────────────
   探测不需要验证码，所以「当前有没有可领的套餐」这条信息是**免费**的；
   而一旦开始领取就要用户拖一次滑块。先探测再确认，能让用户在动手之前就
   知道有什么可领、值不值得拖 —— 也让「今天没有活动」这种最常见的结果
   不消耗一次人工交互。

   ── 404 与「业务失败」都不是错误（后端已归一）───────────────
     · `deployed: false`（上游活动接口未部署）→ 说一句「当前没有可领套餐」；
     · 领取返回 `ok: false` + `failure`（如 `already_claimed` / `quota_exhausted`
       / `captcha`）→ 按 `failureLabel` 提示。这两种都不该报红。
       只有 HTTP 非 2xx（账号不存在 / 缺 jwt / 网络故障）才当失败处理。

   依赖：`window.workbuddyDesktop`（桥接方法，见 web_shim.rs 的
   zcodeClaim* 三个）、`window.wbAliyunCaptcha`、`window.wbConfirm`、
   `window.wbApp.toast`。脚本顺序见 index.html。 */
(() => {
  const bridge = () => window.workbuddyDesktop;

  const describeError = error => {
    if (error instanceof Error && error.message) return error.message;
    const text = String(error ?? '').trim();
    return text || '未知错误';
  };

  /** 一条套餐 → 给人看的一行（探测结果里用它列出可领的东西） */
  function describePlan(plan) {
    const window_ = [formatTime(plan.startsAt), formatTime(plan.endsAt)]
      .filter(Boolean)
      .join(' → ');
    const grants = (plan.entitlements || [])
      .map(item => {
        const quota = Number(item.grantUnits) > 0
          ? ` ${item.grantUnits} ${item.unitType || ''}`.trimEnd()
          : '';
        return `${escapeHtml(item.showName || '')}${quota}`;
      })
      .filter(Boolean);
    return `<li><b>${escapeHtml(plan.name || plan.planId || '套餐')}</b>`
      + (window_ ? `（${escapeHtml(window_)}）` : '')
      + (grants.length ? `<br><span class="muted">${grants.join(' · ')}</span>` : '')
      + (plan.description ? `<br><span class="muted">${escapeHtml(plan.description)}</span>` : '')
      + '</li>';
  }

  /** unix 秒 → 本地可读时间（后端给的一律是秒，不是毫秒） */
  function formatTime(seconds) {
    const value = Number(seconds);
    if (!Number.isFinite(value) || value <= 0) return '';
    try {
      return new Date(value * 1000).toLocaleString();
    } catch {
      return '';
    }
  }

  function escapeHtml(text) {
    return String(text)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  /**
   * 走完一次「探测 → 确认 → 验证码 → 领取」。
   *
   * `account` 是账号行对象（要用它的 `id` 与 `name`）。
   */
  async function start(account) {
    const api = bridge();
    const accountId = String(account?.id || '');
    if (!accountId) {
      window.wbApp.toast('账号信息不完整，请刷新后重试', 'warn');
      return;
    }
    if (!api?.zcodeClaimPreview || !api?.zcodeClaim) {
      window.wbApp.toast('当前环境不支持领取（桥接方法缺失）', 'warn');
      return;
    }

    // ── ① 探测（不要验证码）──────────────────────────────────
    let preview;
    try {
      preview = await api.zcodeClaimPreview(accountId);
    } catch (error) {
      window.wbApp.toast(`探测套餐失败：${describeError(error)}`, 'warn');
      return;
    }
    if (!preview?.deployed) {
      // 上游活动接口还没部署 —— 周末套餐开抢前的**正常**状态，不是错误
      window.wbApp.toast('当前没有可领取的套餐（活动尚未开始）');
      return;
    }
    const plans = Array.isArray(preview.plans) ? preview.plans : [];
    if (plans.length === 0) {
      window.wbApp.toast('当前没有可领取的套餐');
      return;
    }
    // 取优先级最高的那个作为默认目标（后端 planId 为空时也是这个口径）
    const target = [...plans].sort((a, b) => (Number(b.priority) || 0) - (Number(a.priority) || 0))[0];

    // ── ② 让用户看清楚要拖一次滑块，再动手 ───────────────────
    const ok = await window.wbConfirm?.ask?.({
      title: '领取 ZCode 体验套餐',
      html: `账号：<b>${escapeHtml(account.name || accountId)}</b><br>`
        + `可领取的套餐：<ul class="zcode-claim-plans">${plans.map(describePlan).join('')}</ul>`
        + '<p class="muted">官方要求一次人机验证（滑块），完成后即可领取。'
        + '领取是按账号计的，同一期活动重复领取会提示「已领取过」。</p>',
      okText: `领取「${escapeHtml(target.name || target.planId || '套餐')}」`,
    });
    if (!ok) return;

    // ── ③ 要不要滑块，由上游的风控配置说了算 ─────────────────
    // `enabled: false` = 上游此刻不要人机验证 → **直接领**（后端允许不带
    // 验证码参数，只在该有值时发那个头）。硬弹一次滑块会让用户在本不需要
    // 验证的时候被拦一道，而且滑块可能根本初始化不出来。
    let captchaConfig = { enabled: false };
    try {
      captchaConfig = await api.zcodeClaimCaptchaConfig(accountId) || { enabled: false };
    } catch (error) {
      // 取配置失败不等于不能领：按「不需要验证码」试一次，上游要的话会回
      // 3007，那时再如实告诉用户「验证码校验未通过」
      window.wbApp.toast(`获取风控配置失败，将直接尝试领取：${describeError(error)}`, 'warn');
    }

    const callClaim = captchaVerifyParam => api.zcodeClaim(
      accountId,
      target.planId || '',
      captchaVerifyParam || '',
      captchaConfig.region || '',
    );

    // 不需要验证码：一次请求就完事
    if (!captchaConfig?.enabled) {
      try {
        reportOutcome(await callClaim(''));
      } catch (error) {
        window.wbApp.toast(`领取失败：${describeError(error)}`, 'warn');
      }
      return;
    }

    // ── ④ 需要验证码：滑块 → 拿 verifyParam 直接领取 ──────────
    // 注意与 AutoClaw 的 OAuth 不同 —— 那边拿到串之后是去换授权地址、再开窗口
    // 等回调（长流程，必须放在 solve 之外）；这里是**拿串直接领取**，全部动作
    // 都在 `request` 回调里完成，因此不存在「长流程被 120 秒验证码超时误杀」
    // 的问题（见 aliyun-captcha.js 里 solve 的说明）。
    const captcha = window.wbAliyunCaptcha;
    if (!captcha) {
      window.wbApp.toast('验证码组件未加载，请重启应用后重试', 'warn');
      return;
    }

    let outcome;
    try {
      outcome = await captcha.solve(
        {
          region: captchaConfig.region || 'ga',
          prefix: captchaConfig.prefix,
          sceneId: captchaConfig.sceneId,
        },
        async captchaVerifyParam => {
          const result = await callClaim(captchaVerifyParam);
          // 业务结果借返回值带回上层（`solve` 会原样 resolve 它）
          return { captchaResult: true, bizResult: true, result };
        },
      );
    } catch (error) {
      // 用户主动取消验证码不算失败（与 AutoClaw 登录那边同一处置）
      if (error instanceof captcha.CaptchaCancelledError) {
        window.wbApp.toast('已取消领取');
        return;
      }
      window.wbApp.toast(`领取失败：${describeError(error)}`, 'warn');
      return;
    }
    reportOutcome(outcome?.result);
  }

  /**
   * 领取结果 → 提示。
   *
   * 业务失败按后端给的 `failureLabel` 提示（中文已由后端归一，前端不再按
   * `failure` 键自己写一套文案 —— 那样两处会漂移）。
   */
  function reportOutcome(result) {
    if (result?.ok) {
      const window_ = [formatTime(result.startsAt), formatTime(result.endsAt)]
        .filter(Boolean)
        .join(' → ');
      window.wbApp.toast(
        `✅ 领取成功：${result.planId || ''}${window_ ? `（${window_}）` : ''}`,
      );
      return;
    }
    const label = result?.failureLabel || '未领取成功';
    window.wbApp.toast(`${label}${result?.message ? `：${result.message}` : ''}`, 'warn');
  }

  // 「这个账号有没有领取能力」的判据**不在这里** —— 它走账号页的能力表
  // （`accounts-groups.js` 的 `claim` 位 + 后端给的 `canClaim`，
  // 见 `supportsClaim`）。本模块只负责「发起一次领取」。
  window.wbZcodeClaim = { start };
})();
