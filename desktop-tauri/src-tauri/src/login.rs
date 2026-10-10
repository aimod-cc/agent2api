//! 登录流程：拉起登录窗口（内嵌 WebView 或系统浏览器）并轮询后端直到完成。
//!
//! 后端把登录拆成三步：start 拿 state+authUrl → 用户在页面上完成 →
//! 轮询 wait 拿结果。桌面端只负责托管页面与轮询，不接触任何凭证。
//!
//! ── 三家 provider 的三条链（为什么这里要分流）────────────────
//!   - **workbuddy**：后端向自己上游要 state/authUrl，凭证由后端轮询上游拿回，
//!     桌面端**完全没有回调要处理**。系统浏览器模式（external）也能用，
//!     因为用户在哪登录都行 —— 判定归后端。
//!   - **小浣熊**：官方登录页在登录成功后跳转自定义协议
//!     `office-raccoon://auth/callback?code=…&state=…`，那个 code 必须由网关
//!     换成凭证（code 是给服务端的，不是给用户的）。**难点在于谁能拿到那个 URL**：
//!     Electron 版能在会话里 `session.protocol.handle('office-raccoon', …)` 接管
//!     自定义协议，Tauri 没有这个能力（Tauri 的 custom-protocol 是给自己 webview
//!     加载前端资源用的，不是系统级 URL scheme 注册），WebView2 的
//!     NavigationStarting 也只在**导航发生前**问一句要不要拦。因此这里改成：
//!     在导航拦截里认出回调 URL → **拦下导航** → 把 URL 原样 POST 给后端
//!     （`/api/session/login/callback`），由此完成「窗口自己捕获回调」。
//!     也正因如此，小浣熊**只提供内嵌窗口**：系统浏览器模式下那个深链要靠
//!     系统注册 `office-raccoon://` 才回得来（那是官方客户端注册的，装了才有），
//!     给用户一个「大概率永远收不到回调」的选项只会制造难查的卡死。
//!   - **Qoder**：设备授权（PKCE）——授权页只负责把设备码交给用户点的那个账号，
//!     凭证由网关轮询设备令牌接口取回（`providers::qoder::oauth`），桌面端同样
//!     没有回调。内嵌与系统浏览器两种方式都可选（前者用全新的临时数据目录，
//!     所以能连着添加多个互不影响的账号）。
//!   - **CatPaw**：上游把 `{token, state}` 表单 **POST 到本机网关**的 loopback
//!     端口，壳侧同样没有回调要处理（见 `core::login::catpaw`）。
//!   - **AutoClaw 国际版（OAuth）**：与 CatPaw 同一形态（回调落在本机网关的
//!     loopback 端口），但**发起顺序是倒的** —— 授权地址由**前端**拿好再交给壳
//!     （它前面有一次必须在浏览器里跑完的强制风控验证码，见
//!     [`start_autoclaw_oauth`] 的说明），所以它不走 `start()` 那条
//!     「壳问后端要 authUrl」的路，而是由 `start_autoclaw_oauth_login` 这条
//!     命令直接接收 `{state, authUrl}`。
//!
//! ── 凭什么三个 provider 共用同一段轮询 ────────────────────
//! 三家的差别只有「授权地址怎么来」与「谁算登录成功」，两者都被后端收进了
//! `LoginTask`：壳只做「打开 URL → 每 2 秒问一次 `/wait` → 按结果收尾」。
//! 唯一的壳侧差异是回调拦截（只有小浣熊需要），因此 workbuddy 与 Qoder 走同一条
//! 内嵌窗口路径；内嵌窗口本身对三家一视同仁地**每次新建临时数据目录**（见
//! `login_profile.rs`：这是「添加第二个账号时不复用上一个账号登录态」的关键）。
//!
//! ── User-Agent 为什么**不需要**清洗（与原 Electron 版的差别）─────
//! Electron 版的登录窗口带 `Electron/xx` 段，源实现特意把它抹掉（有些登录页会
//! 按 UA 拦非浏览器客户端）。Tauri/WebView2 这边没有这个问题：壳没有覆盖 UA
//! （`WebviewWindowBuilder::user_agent` 没被调用，wry 也不追加任何壳标识），
//! 于是登录窗口用的是 **WebView2 自己的标准 Edge UA**
//! （`Mozilla/5.0 … Chrome/… Edg/…`），本来就是一个普通浏览器的形态。
//! 主动去改它反而有两个代价：一是要自己拼一份 UA（版本号会过期），
//! 二是「UA 与 CH-UA 头不一致」在部分站点的风控里比「多一个 Electron 段」更显眼。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::gateway;
use crate::login_profile::LoginProfile;
use crate::state::ActiveLogin;

pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// WorkBuddy 国际版登录页「只留 OneID/邮箱」的强制样式 id。
///
/// 上游前端（`index-*.js`）在 `enable_oneid_only_login` 为 true 时，把这段样式
/// 注入登录 iframe 的 head，隐藏 `#kc-social-providers`（Google / GitHub / X
/// 三个按钮的容器）并强制切到邮箱 tab。开关来自：
///   GET https://www.workbuddy.ai/v2/plugin/login/entry-policy
///   → {"data":{"enable_oneid_only_login":true}}
/// 按钮本身与 Keycloak 的 broker 通道都是好的（实测 `broker/google/login`
/// 能正常跳 accounts.google.com），所以摘掉这段样式即可恢复入口。
const SOCIAL_HIDDEN_STYLE_ID: &str = "oneid-only-login-style";

/// 摘掉上面那段样式，恢复 Google / GitHub / X 入口。
///
/// ── 为什么用 `for_all_frames` 注入 ──────────────────────────
/// 登录页是**两层**结构：外层 `www.workbuddy.ai/login` 只是个壳，真正的
/// Keycloak 授权页在内层 iframe 里，样式也由外壳注进那个 iframe。注入脚本要
/// 在 iframe 的文档里生效，就必须用 `initialization_script_for_all_frames`
/// （wry 在 Windows 上走 `AddScriptToExecuteOnDocumentCreated`，覆盖所有 frame）。
///
/// ── 为什么是「反复摘」而不是「摘一次」 ────────────────────────
/// 注入不止一次：每次 iframe 重新加载（登录中途跳转回来、语言切换）外壳都会
/// 再插一遍，且注入方先判断「有没有这个 id」——所以我们必须**持续**盯着并移除，
/// 而不是只处理一次。用一个 setInterval 轮询是最省事且不依赖 DOM 事件顺序的做法：
/// 样式可能在 DOMContentLoaded 之前就进了 head，事件监听容易错过那一拍。
/// 间隔 200ms 只做一个 getElementById，开销可以忽略。
///
/// ── 边界 ──────────────────────────────────────────────────
///   1. 只在 `workbuddy.ai` 域名上动手：登录窗口会跳到 accounts.google.com /
///      github.com，那些页面不该被注入任何东西。
///   2. 只移除**那一个 id** 的 style，不碰别的节点，也不改 `switchTab` 行为 ——
///      用户的默认落点仍是上游决定的那个 tab，我们只是不再隐藏入口。
fn social_restore_script() -> String {
    // 样式 id 从常量注入，避免这里再写一份字面量（两处一旦不一致就是
    // 「脚本在跑但什么都没摘掉」这种没有任何报错的静默失效）。
    format!(
        r#"
(function () {{
  var STYLE_ID = '{style_id}';
  if (!/(^|\.)workbuddy\.ai$/.test(location.hostname)) return;
  function strip() {{
    var el = document.getElementById(STYLE_ID);
    if (el && el.parentNode) el.parentNode.removeChild(el);
  }}
  strip();
  setInterval(strip, 200);
}})();
"#,
        style_id = SOCIAL_HIDDEN_STYLE_ID,
    )
}

/// KukuAI 短信自动流程脚本：自动填手机号、点发码、等验证码、自动填码登录。
///
/// ── 登录态交回不由脚本做 ─────────────────────────────────────
/// passport 登录成功后跳转 `success.html`，`BDUSS` 是 **HttpOnly** Cookie，
/// `document.cookie` 读不到 —— 交回由壳侧在 `run_embedded` 的等待循环里
/// 检测 `success.html` 跳转后读 WebView2 的 Cookie 存储完成
/// （`cookies()` 含 HttpOnly，见 [`submit_kuku_login`]）。脚本只负责
/// 「替用户点」：手机号由 `start_kuku_sms` 带进来，验证码由
/// `submit_kuku_sms_code` 经 `window.eval("window.__kukuSmsCode='…'")`
/// 写入页面变量（脚本轮询到就填）。
///
/// ── 图形验证码天然兼容 ─────────────────────────────────────
/// passport 对异常 IP / 高频可能要求图形验证码（页面上多一个输入框）。脚本
/// 检测到验证码输入框可见就经 `document.title` 上报（壳的监视任务据此把
/// 隐藏窗口亮出来），用户填完图形验证码后短信正常发出，脚本继续自动走。
///
/// ── React 受控输入 ─────────────────────────────────────────
/// passport 表单是 React 受控组件，直接 `el.value = x` 不会进 React 状态，
/// 必须走原生 value setter + `input` 事件（页面结构用的是 `TANGRAM__PSP_4`
/// 前缀，实测 `loginMerge.html` 与通用 passport 弹窗同一套 DOM，
/// 见 asar 对读与 2026-10-07 浏览器实测）。
fn kuku_sms_script(phone: &str) -> String {
    format!(
        r#"
(function () {{
  // ── 防重复注入 ──────────────────────────────────────────────
  // 壳侧会**周期性重新注入**本脚本（initialization_script 的注入时机在
  // WebView2 首次导航时可能早于 location 就绪，只靠它脚本有没跑起来的
  // 风险；周期性 eval 兜底）。同一 document 内重复执行由这个 flag 拦住；
  // 页面导航后 window 重置，flag 自然清零、脚本重新初始化。
  if (window.__kukuInjected) return;
  window.__kukuInjected = true;
  var PHONE = '{phone}';
  // ── 宽容选择器（不写死 TANGRAM__PSP_4 前缀）──────────────────
  // 组件实例编号随页面版本浮动（PSP_4 只是 loginMerge.html 当前值），只有
  // `__smsPhone` 这类**后缀**是模板层稳定的。实测（2026-10-07）手机号框
  // name 是 username、动态密码框 name 是 password，与账密表单重名 ——
  // 所以**不按 name 匹配**，一律走 id 后缀。
  function q(sel) {{ return document.querySelector(sel); }}
  function findAll(sel) {{ return Array.prototype.slice.call(document.querySelectorAll(sel)); }}
  function phoneInput() {{ return q('[id$="__smsPhone"]'); }}
  function codeInput() {{ return q('[id$="__smsVerifyCode"]'); }}
  function sendBtn() {{
    return findAll('button, a').find(function (n) {{
      return /发送动态密码|获取验证码/.test(n.textContent || '');
    }});
  }}
  function submitBtn() {{ return q('[id$="__smsSubmit"]'); }}
  // ── 状态上报（壳侧监视任务读 document.title 判定，见 start_kuku_sms）──
  // eval 拿不到页面变量（fire-and-forget），title 是壳能同步读的通道。
  // 中间态（READY/FILLED/CLICKED）让监视任务与前端能看出脚本走没走。
  var T_READY = 'WB_KUKU_READY';          // 脚本已启动（页面加载完成）
  var T_FILLED = 'WB_KUKU_PHONE_FILLED';  // 手机号已填入
  var T_CLICKED = 'WB_KUKU_SEND_CLICKED'; // 已点「发送动态密码」
  var T_SENT = 'WB_KUKU_SMS_SENT';        // 短信已发出：保持隐藏等用户填码
  var T_CAPTCHA = 'WB_KUKU_NEED_CAPTCHA'; // 需要图形验证码：亮窗让用户填
  var T_LOGIN_OK = 'WB_KUKU_LOGIN_OK';    // 登录成功（同页「提交成功」形态）
  function report(title) {{
    try {{ document.title = title; }} catch (e) {{}}
  }}
  function loginDone() {{
    // loginMerge.html 登录成功**不跳转**，停在本页显示「好开心，提交成功啦！」
    // （实测 2026-10-07）。壳侧据此触发交回（BDUSS 由壳读 Cookie 存储）。
    var body = (document.body && document.body.innerText) || '';
    return /提交成功|登录成功/.test(body);
  }}
  function sendDone() {{
    // 发码成功的标志：发送按钮进入冷却倒计时（文本变成「重新发送(48)」
    // 或含数字+s 的倒计时，且 disabled）。注意**不能**再用「发送动态密码」
    // 匹配 —— 发码后按钮文本就变了，用旧正则永远认不出（真实 bug）。
    var b = findAll('button').find(function (n) {{
      return n.disabled && /重新发送|\d+\s*s/.test(n.textContent || '');
    }});
    return Boolean(b);
  }}
  function captchaBoxVisible() {{
    // 图形验证码输入框（passport 风控弹窗里的 verifyCode）。
    // 用户名登录表单也常驻一个 verifyCode 框 —— 但短信流程点发码后
    // 若按钮已进入倒计时（短信已发），那个框与短信无关，见下方顺序。
    var boxes = findAll('input[id*="verifyCode"], input[id*="vcode"]');
    var v = boxes.find(function (n) {{
      var style = window.getComputedStyle(n);
      return style.display !== 'none' && style.visibility !== 'hidden';
    }});
    return Boolean(v);
  }}
  var timer = setInterval(function () {{
    try {{
      // 还没导航到百度域（脚本可能先注入在 about:blank）：等下一轮。
      // 检查放在轮询里而不是脚本顶层 —— 顶层检查会把「注入早于 location
      // 就绪」的这一次执行整个吃掉（真实踩过：脚本静默不工作）。
      if (location.hostname.indexOf('baidu.com') === -1) return;
      // 0. 状态上报（每轮都写，页面不会自己改 title）。顺序重要：
      //    登录成功（页面出现「提交成功」）最优先 —— 此时发送按钮倒计时
      //    可能还在，不能让它盖掉登录成功信号；短信已发（倒计时）次之
      //    （此时即使页面有可见验证码框也**不是**拦截，实测 2026-10-07）。
      if (loginDone()) {{
        report(T_LOGIN_OK);
      }} else if (sendDone() || window.__kukuSentOk) {{
        window.__kukuSentOk = true;
        report(T_SENT);
      }} else if (captchaBoxVisible()) {{
        report(T_CAPTCHA);
      }} else if (!window.__kukuReady) {{
        window.__kukuReady = true;
        report(T_READY);
      }}
      // 1. 切到「短信快捷登录」tab（点一次即可；tab 不存在说明页面已是短信形态）
      if (!window.__kukuTab) {{
        var tab = q('a.pass-sms-btn') || findAll('a, span, div').find(function (n) {{
          return /短信快捷登录|短信登录/.test(n.textContent || '');
        }});
        if (tab) {{ window.__kukuTab = true; tab.click(); }}
      }}
      // 2. 填手机号 + 点「发送动态密码」
      var phone = phoneInput();
      if (phone && !window.__kukuSent) {{
        window.__kukuSent = true;
        report(T_FILLED);
        var setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
        setter.call(phone, PHONE);
        phone.dispatchEvent(new Event('input', {{ bubbles: true }}));
        setTimeout(function () {{
          var b = sendBtn();
          if (b) {{
            report(T_CLICKED);
            b.click();
          }}
        }}, 400);
      }}
      // 3. 填验证码 + 点「登录并添加」所在的提交按钮
      var code = codeInput();
      if (code && window.__kukuSmsCode) {{
        // 填码只做一次（页面不清空就不用重填）；点提交按节流重试 ——
        // passport 的提交按钮填码瞬间可能还 disabled（风控校验没走完），
        // 一次性 flag 会永远错过那次点击。
        if (!window.__kukuCodeFilled) {{
          window.__kukuCodeFilled = true;
          var setter2 = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
          setter2.call(code, window.__kukuSmsCode);
          code.dispatchEvent(new Event('input', {{ bubbles: true }}));
        }}
        var sub = submitBtn();
        if (sub && !sub.disabled && !window.__kukuSubmitting) {{
          window.__kukuSubmitting = true;
          sub.click();
          // 若点击无效（按钮校验后恢复），8 秒后允许再试一次
          setTimeout(function () {{ window.__kukuSubmitting = false; }}, 8000);
        }}
      }}
      // 不做次数上限：脚本随窗口存活，登录完成（壳读到 Cookie 后任务 Done）
      // 或窗口销毁才停 —— 用户拖很久才填码 / 手动补图形验证码时仍然要活着。
    }} catch (e) {{}}
  }}, 500);
}})();
"#,
        phone = phone,
    )
}

/// KukuAI 短信自动流程的发起：打开登录窗口（注入自动脚本）后**立即返回**，
/// 窗口在后台轮询 —— 验证码由 [`submit_kuku_sms_code`] 在用户收码后写入。
///
/// ── 为什么不在这里等完成 ────────────────────────────────────
/// 短信登录是「发码 → 收码 → 输码」两步交互，发码后窗口必须继续等用户输码；
/// 发起命令一等到完成，前端就永远等不到 verify 那一步了。因此这里
/// **spawn 后台等待循环**（复用 `run_embedded`：等 `/wait` Done / 失败 / 超时
/// 后自行关窗），命令本身立刻返回。
pub async fn start_kuku_sms(app: &AppHandle, phone: &str) -> Result<serde_json::Value, String> {
    if current_login(app).is_some() {
        return Err("已有登录流程在进行，请先完成当前登录或等待超时".to_string());
    }
    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "provider": "kuku" })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: String::new(),
            mode: "sms".into(),
            provider: "kuku".to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some("sms".into()),
            edition: None,
            provider: Some("kuku".to_string()),
        },
    );

    // 后台并行两条：
    //   1. run_embedded —— 窗口全程隐藏，脚本自动发码 / 等码 / 登录，
    //      完成后（Done / 失败 / 超时）自己关窗并清状态；
    //   2. 窗口监视 —— 只负责「需要人工介入时亮窗」：
    //      - 脚本上报「需要图形验证码」（页面出现验证码输入框）→ 立即亮窗，
    //        用户填完图形验证码后短信发出，脚本继续自动走；
    //      - 90 秒没等到发码确认（页面加载慢 / 结构变化等）→ 兜底亮窗，
    //        让用户直接在那个窗口里手动完成登录（脚本仍会交回 BDUSS）。
    //   失败只记日志：真正的文案在后端任务里，`submit_kuku_sms_code` 轮询
    //   `/wait` 时会读到并透给前端。
    let app_for_task = app.clone();
    let login_state_for_task = login_state.clone();
    let phone_for_task = phone.to_string();
    let monitor_label = login_window_label(&login_state_for_task);
    tauri::async_runtime::spawn(async move {
        let monitor = async {
            // 每 1 秒读一次窗口标题（脚本经 document.title 上报状态）。
            // 三级兜底，不让用户干等：
            //   - 10 秒内窗口没有任何脚本状态 → 页面没加载 / 脚本没启动，
            //     立即亮窗（用户能看到真实页面）；
            //   - 20 秒内没确认发码成功 → 发码可能被风控拦，亮窗；
            //   - 30 秒最终兜底亮窗。
            // 每次状态变化：壳日志 + `login:kuku-status` 事件推给前端
            // （面板提示行实时显示进度，用户不再「点了没反应」）。
            let mut last_seen = String::new();
            let emit_status = |status: &str| {
                let _ = app_for_task.emit("login:kuku-status", json!({ "status": status }));
            };
            let mut saw_script = false;
            for tick in 0..30 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(window) = app_for_task.get_webview_window(&monitor_label) else {
                    return; // 窗口已销毁（登录完成/取消），无事可监
                };
                let title = window.title().unwrap_or_default();
                if title != last_seen {
                    eprintln!("[login] KukuAI 短信窗口状态: {title}");
                    last_seen = title.clone();
                    match title.as_str() {
                        "WB_KUKU_SMS_SENT" => {
                            emit_status("sent");
                            return; // 短信已发出：保持隐藏，等用户填码
                        }
                        "WB_KUKU_NEED_CAPTCHA" => {
                            emit_status("captcha");
                            let _ = window.show();
                            return;
                        }
                        "WB_KUKU_READY" => {
                            saw_script = true;
                            emit_status("opening");
                        }
                        "WB_KUKU_PHONE_FILLED" => {
                            saw_script = true;
                            emit_status("filling");
                        }
                        "WB_KUKU_SEND_CLICKED" => {
                            saw_script = true;
                            emit_status("sending");
                        }
                        _ => {}
                    }
                }
                // 10 秒内脚本没有任何动静：页面没加载 / 脚本没注入 → 亮窗
                if tick == 10 && !saw_script {
                    eprintln!("[login] KukuAI 短信窗口 10 秒无脚本状态，亮窗让用户手动完成");
                    emit_status("timeout");
                    let _ = window.show();
                    return;
                }
            }
            // 30 秒未确认发码：亮窗兜底（用户可手动完成，登录态仍由壳读回）
            if let Some(window) = app_for_task.get_webview_window(&monitor_label) {
                eprintln!("[login] KukuAI 短信发码 30 秒未确认，亮窗让用户手动完成");
                emit_status("timeout");
                let _ = window.show();
            }
        };
        let run = run_embedded(
            &app_for_task,
            "kuku",
            &auth_url,
            "登录 KukuAI 账号",
            &login_state_for_task,
            false,
            Some(&phone_for_task),
            true, // start_hidden：窗口隐藏，全程脚本后台操作
        );
        let (result, ()) = tokio::join!(run, monitor);
        if result.as_ref().map(|value| value.get("ok").and_then(serde_json::Value::as_bool)) != Ok(Some(true)) {
            let _ = gateway::call(
                "POST",
                "/api/session/login/cancel",
                Some(&json!({ "state": login_state_for_task.clone() })),
            )
            .await;
        }
        clear_active_login(&app_for_task, &login_state_for_task);
        if let Err(error) = result {
            eprintln!("[login] KukuAI 短信登录后台流程结束: {error}");
        }
    });

    Ok(json!({ "state": login_state }))
}

/// KukuAI 短信自动流程的收码：把用户收到的验证码写入登录窗口，等登录完成。
///
/// 写入方式：`window.eval("window.__kukuSmsCode='…'")` —— 窗口脚本每 500ms
/// 轮询这个变量，读到就自动填码点登录（见 [`kuku_sms_script`] 第 3 步）。
///
/// 然后轮询 `/wait` 直到后端任务 Done（登录完成、账号已落）或失败 / 超时；
/// 完成后调 `/api/accounts` 取该账号返回 `{account, list}`（与
/// `POST /api/accounts` 的响应同形状，前端直接交给「已添加账号」收尾逻辑）。
pub async fn submit_kuku_sms_code(
    app: &AppHandle,
    code: &str,
) -> Result<serde_json::Value, String> {
    let active = current_login(app).ok_or("没有进行中的 KukuAI 短信登录，请先点「获取验证码」")?;
    if active.provider != "kuku" || active.mode != "sms" {
        return Err("没有进行中的 KukuAI 短信登录，请先点「获取验证码」".to_string());
    }
    let window_label = login_window_label(&active.state);
    let Some(window) = app.get_webview_window(&window_label) else {
        return Err("KukuAI 登录窗口已关闭，请重新发起短信登录".to_string());
    };
    let code = code.trim().to_string();
    if code.len() != 6 || !code.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("请填写 6 位数字验证码".to_string());
    }
    // 等登录完成（与 run_embedded 的等待循环同一出口：/wait 三态）
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("等待登录完成超时，请重试".to_string());
        }
        // 每轮重写验证码变量（fire-and-forget）：passport 页面可能在发码后
        // 刷新（图形验证码 / 风控重渲染），一次性写入会随旧页面一起丢 ——
        // 窗口脚本每 500ms 轮询这个变量，新页面里读到就自动填码提交。
        let _ = window.eval(&format!("window.__kukuSmsCode = '{}';", code));
        tokio::time::sleep(POLL_INTERVAL).await;
        match poll_once(&active.state).await {
            PollOutcome::Pending => continue,
            PollOutcome::Done(_) => break,
            PollOutcome::Failed(error) => return Err(error),
            PollOutcome::Unreachable(error) => {
                if task_gone(&error) {
                    return Err("登录已被取消或过期，请重新发起".to_string());
                }
                eprintln!("[login] 读取登录进度失败（继续等待）: {error}");
                continue;
            }
        }
    }
    // 完成：取账号列表，挑本次落的 KukuAI 账号返回（与添加账号响应同形状）
    let accounts = gateway::call("GET", "/api/accounts", None)
        .await
        .map_err(|error| format!("登录完成，但读取账号列表失败：{error}"))?;
    let list = accounts
        .get("list")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let account = list
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .filter(|item| item.get("provider").and_then(serde_json::Value::as_str) == Some("kuku"))
                .max_by_key(|item| item.get("addedAt").and_then(serde_json::Value::as_i64).unwrap_or(0))
                .cloned()
        })
        .unwrap_or_else(|| serde_json::json!({ "provider": "kuku", "name": "KukuAI 账号" }));
    Ok(json!({ "account": account, "list": list }))
}

/// workbuddy 登录窗口允许导航的域名，取自两版 cli/product.json 的
/// internalDomain / externalDomain / iOADomain，外加扫码登录所需的微信/QQ 域名。
///
/// **第三方身份提供方（Google / GitHub / X）不在这张表里**：它们是可选入口，
/// 只有勾选「恢复第三方入口」时才额外放行，见 [`SOCIAL_IDENTITY_HOSTS`]
/// 与 [`host_allowed`]。
const WORKBUDDY_ALLOWED_HOSTS: &[&str] = &[
    "copilot.tencent.com",
    "staging-copilot.tencent.com",
    "codebuddy.cn",
    "workbuddy.cn",
    "codebuddy.ai",
    "staging-codebuddy.tencent.com",
    "workbuddy.ai",
    "staging.workbuddy.ai",
    "tencent.com",
    "qq.com",
    "wechat.com",
    "weixin.qq.com",
    "tenpay.com",
];

/// 「恢复 Google / GitHub 入口」勾选时额外放行的第三方身份提供方域名。
///
/// ── 为什么必须放行 ────────────────────────────────────────
/// 恢复出来的按钮点击后**不在 iframe 内跳转**：登录页的 `handleAuthLogin` 做的是
/// `window.parent.location.href = url`（顶层跳转，实测）。也就是整个登录窗口会
/// 走去 Google / GitHub，登录完成后再经 Keycloak broker 回调跳回 workbuddy.ai。
/// 这些域名不在白名单里就会被 `host_allowed` 静默拦下，症状是「点了 Google 登录
/// 窗口一片空白」——正是 `allowed_hosts` 注释里警告过的那类难查故障。
///
/// ── 名单怎么来的 ──────────────────────────────────────────
/// 实测点击 Google 登录后的落点是 `accounts.google.com`（授权页），GitHub 同理
/// 走 `github.com`。其余条目是两家登录链路上的常规跳转与静态资源域
/// （Google 的 gstatic 承载登录页资源，GitHub 的 githubusercontent 承载头像等），
/// 少了它们会出现「页面能到、但资源加载不全」的半残状态。
const SOCIAL_IDENTITY_HOSTS: &[&str] = &[
    "accounts.google.com",
    "accounts.youtube.com",
    "google.com",
    "gstatic.com",
    "googleusercontent.com",
    "github.com",
    "githubusercontent.com",
    "githubassets.com",
    "twitter.com",
    "x.com",
    "twimg.com",
];

/// 小浣熊登录窗口允许导航的域名。
///
/// ── 为什么不是「只有 xiaohuanxiong.com」──────────────────────
/// 登录页本身在 `https://xiaohuanxiong.com/code/authorize`，但它把验证码 / 滑块
/// 之类的交互托管在第三方（阿里云、腾讯、百度都有），跳过去再跳回来是正常路径。
/// 所以这份清单**逐字取自原项目的 `LOGIN_ALLOWED_HOSTS`**（`desktop/main.cjs`）——
/// 那是同一条登录链路上经过实际使用验证的白名单，不是猜的；凭「同域就够」的
/// 判断砍成一个域名，会让「验证码加载不出来、登录窗口停在白屏」变成一个
/// 只有用户能碰到的新故障。
///
/// `RACCOON_MAIN_SITE_URL` 指向别处（测试环境）时这份清单**不跟着变**：
/// 一处能改导航白名单的配置项等于没有白名单，测试环境请改这里并重新编译。
const RACCOON_ALLOWED_HOSTS: &[&str] = &[
    "xiaohuanxiong.com",
    "sensetime.com",
    "aliyun.com",
    "aliyuncs.com",
    "alicdn.com",
    "qq.com",
    "baidu.com",
];

/// 小浣熊回调的形态（`office-raccoon://auth/callback`），与后端
/// `raccoon::oauth` 里的三个常量必须一致（那边负责最终校验，这里只做识别）。
const RACCOON_CALLBACK_SCHEME: &str = "office-raccoon";
const RACCOON_CALLBACK_HOST: &str = "auth";
const RACCOON_CALLBACK_PATH: &str = "/callback";

/// Qoder 与 CatPaw 登录窗口允许导航的域名：**不限制**。
///
/// ── 为什么这两家不设域名白名单 ───────────────────────────────
/// 两家都会跳出自己的域，且跳转主机不可穷举：
///   - Qoder 的登录入口按用户选择跳到第三方身份提供方（Google / GitHub），
///     授权页本身在 `qoder.com`，完整 SSO 链路的跳转主机列不全；
///   - CatPaw 走美团 passport（`passport.meituan.com` → `settoken` →
///     `catpaw.meituan.com` 的 `login-callback`），沿途还会跳微信 / 支付宝
///     这类扫码登录，第三方名单同样没有尽头。
///
/// 而 `on_navigation` 返回 false 是**静默拦下**：漏一个主机就表现为「窗口打开
/// 但一片空白」或「点了登录没反应」这种极难排查的故障 —— CatPaw 首次接入时
/// 正是因为漏了这一分支、落进默认分支拿到 WorkBuddy 的白名单而白屏。
///
/// 放行的代价在这两家可以接受：两者的凭证都换不出「别人手里的东西」——
/// Qoder 的授权码要靠只在网关进程内的 PKCE verifier 才能兑换（见
/// `providers::qoder::oauth`）；CatPaw 的 token 由上游直接 POST 到本机网关的
/// loopback 端口，且回调要过一次性 state 逐字校验（见 `core::login::catpaw`）。
/// 且窗口用的是独立临时数据目录（`login_profile`），不携带本机任何登录态。
///
/// ── 为什么 workbuddy / 小浣熊仍然白名单 ─────────────────────
/// 那两家的登录链路是固定的几个站点（腾讯系 / 商汤系 + 各自的验证码托管方），
/// 能列全；白名单在这里是免费的额外一层约束，就不放弃。
///
/// ── AutoClaw 国际版为什么不设限（本次新增，务必读）─────────────
/// 它的登录页与身份提供方都不在我们的名单里（Zai 的授权页、Google 的
/// `accounts.google.com`），而**更要紧的是回调落在本机**：授权完成后浏览器会
/// **顶层导航到 `http://localhost:<登记端口>/auth/callback-zai|google`**
/// （z.ai 的白名单只认官方客户端那四个端口，见
/// `providers::autoclaw::callback_server`）。
/// 那个 loopback 地址一旦被白名单拦下，症状是「用户明明登录成功、网关却永远
/// 等不到授权码」—— 与 [`is_login_callback`] 注释里警告过的那类静默故障同形，
/// 而这次连回调识别都救不了它：回调本身就是一次普通 HTTP 导航，
/// 没有任何「非本机协议」的特征可供识别，只能在白名单这一层放行。
/// （[`autoclaw_callback_forward`] 会把这类导航改成访问网关自己，因此那一步
/// 也不能被白名单挡住 —— 它同样发生在放行之后。）
fn allowed_hosts(provider: &str) -> Option<&'static [&'static str]> {
    match provider {
        "raccoon" => Some(RACCOON_ALLOWED_HOSTS),
        // CatPaw / Qoder / Cline / AutoClaw 都不设限（理由见上）；**新加
        // provider 时不要让它落到默认分支** —— 那会静默沿用 WorkBuddy 的
        // 白名单，症状是登录窗口白屏（AutoClaw 那家还会连回调一起拦掉）。
        //
        // Cline 尤其要注意：它的授权页在 `authkit.cline.bot`，而确认动作可能被
        // 身份提供方接管（WorkOS AuthKit 会按账号配置跳到 Google / GitHub / SSO
        // 等不可穷举的主机）—— 与 Qoder 同一情形，白名单列不全。
        //
        // AutoClaw 的两个地区**都列出来**：国内版目前走不到这条链（它没有
        // 网页登录），但一起列上是有意的 —— 只列国际版的话，哪天国内版也接上
        // OAuth 就会落进默认分支拿到 WorkBuddy 的白名单，而那个故障极难查。
        // Accio 的两个地区也都不设限：登录站点（www.accio.com / www.accio-ai.com）
        // 可能把用户交给不可穷举的身份提供方（Google / 阿里账号 / 手机验证码链路），
        // 与 Qoder / AutoClaw 同一情形。
        //
        // ZCode 的两个地区同样不设限：授权页在 `zcode.z.ai`，而它按用户选择的
        // 登录方式（Z.ai 账号 / Google / 国内版手机号或邮箱）继续跳到各自的身份
        // 提供方，主机不可穷举。**必须显式列出** —— 落进默认分支会拿到 WorkBuddy
        // 的白名单，症状正是上面警告的那种：窗口一片空白，而日志上什么也看不出。
        // 两个地区都列：它们共用同一个授权域，任缺一个都会在将来复用时踩到。
        //
        // CodeArts 同样不设限：授权页在 `codearts.huaweicloud.com/portal/authorize`，
        // 它会按用户选的登录方式继续跳华为云账号（`account.huaweicloud.com`）、
        // IAM 委托、扫码等不可穷举的主机；最后还要回到**网关自己的** loopback 端口
        // 拿授权码（portal 只认我们给的 port，回调路径由它拼死，见
        // `providers::codearts::oauth::authorize_url`）。那条 loopback 导航一旦被
        // 白名单拦下，症状就是上面警告过的那种：用户在官方页面明明登录成功，
        // 网关却永远等不到码。
        //
        // Trae 必须显式列在这里：落进默认分支会拿到 WorkBuddy 的白名单，
        // 那张表**既没有** `trae.cn`（授权页）**也没有** `127.0.0.1`（回调），
        // 症状正是本文件上面警告的那种 —— 窗口一片空白，日志什么也看不出。
        //
        // Antigravity 同样不设限：授权页在 `accounts.google.com`，而 Google 账号
        // 可能继续跳不可穷举的身份链路（二次验证 / 组织 SSO 等）；更要紧的是
        // 回调落在**网关自己的** loopback 端口
        // （`http://localhost:{网关端口}/oauth-callback`，见
        // `providers::antigravity::oauth`）。那条 loopback 导航一旦被白名单拦下，
        // 症状就是上面警告过的那种静默故障：用户明明授权成功，网关却永远等不到码
        // —— 回调本身就是一次普通 HTTP 导航，没有可供回调识别兜底的协议特征。
        "catpaw" | "qoder" | "qoder-intl" | "cline-free" | "cline-pass" | "autoclaw"
        | "autoclaw-intl" | "accio" | "accio-cn" | "zcode" | "zcode-intl" | "codearts" | "trae"
        | "kuku" | "antigravity" => None,
        //
        // OfficeAce 同样不设限：授权页在 `auth.huaweicloud.com`，它会按用户选的
        // 登录方式继续跳华为云账号 / 扫码等不可穷举的主机（与 CodeArts 同一情形）。
        // **不用回本机** —— 授权码由网关**轮询云端**取回（`/v1/claw/auth/code`，
        // 见 `providers::officeace::oauth` 的模块头），所以这里没有 CodeArts 那种
        // loopback 回调要放行；但「不设限」仍是必须显式写出的选择（落进默认分支
        // 会拿到 WorkBuddy 的白名单 → 窗口白屏）。
        "catpaw" | "qoder" | "qoder-intl" | "cline-free" | "cline-pass" | "autoclaw"
        | "autoclaw-intl" | "accio" | "accio-cn" | "zcode" | "zcode-intl" | "codearts" | "trae"
        | "kuku" | "antigravity" | "officeace" => None,
        // WorkBuddy 的两个地区共用这一张表（表里 `workbuddy.ai` 那一行就是国际站
        // 的登录域）。**显式列出**而不是靠下面的默认分支：上面那条警告要求
        // 「新 provider 落到默认分支」必须是有意的选择，写出来才看得出是选过的
        // （拆家后 `workbuddy-intl` 是新 id，靠默认分支时读者无从判断）。
        "workbuddy" | "workbuddy-intl" => Some(WORKBUDDY_ALLOWED_HOSTS),
        _ => Some(WORKBUDDY_ALLOWED_HOSTS),
    }
}

/// 该 URL 是否允许在登录窗口里导航。
///
/// 注意 `office-raccoon:` 这类自定义协议在这里**一律返回 false**，所以导航拦截里
/// 必须先判回调再判白名单 —— 顺序反了会把回调当成非法导航拒掉，而那种拒绝
/// 和「拦下回调」在 WebView2 眼里是同一个动作，日志里看不出区别，
/// 最终表现为「用户登录成功了但网关一直没拿到 code」。
///
/// `social_restore` 勾选时对 WorkBuddy 国际版额外放行 [`SOCIAL_IDENTITY_HOSTS`]：
/// 恢复出来的 Google / GitHub 按钮做的是顶层跳转，不放行就会白屏（理由见该常量）。
fn host_allowed(url: &url::Url, provider: &str, social_restore: bool) -> bool {
    match url.scheme() {
        "about" | "data" => true,
        "https" | "http" => {
            let Some(allowed) = allowed_hosts(provider) else {
                // 不限制主机（Qoder：身份提供方不可穷举，理由见 allowed_hosts）
                return true;
            };
            let Some(host) = url.host_str() else {
                return false;
            };
            let host = host.to_lowercase();
            let matched = |list: &[&str]| {
                list.iter()
                    .any(|item| host == *item || host.ends_with(&format!(".{item}")))
            };
            // `social_restore` 到这里已经是**最终答案**（调用方把「WorkBuddy
            // 系 + 国际版 + 用户勾选」三件事合成一个布尔值传进来，见
            // `run_embedded` 的调用点），这里不再重复判 provider —— 重复判的
            // 后果是拆家后国际版（`workbuddy-intl`）被挡住，Google / GitHub
            // 按钮点下去白屏。
            matched(allowed)
                || (social_restore && matched(SOCIAL_IDENTITY_HOSTS))
        }
        // 非 http(s) 的其它协议一律拒绝（除了上面的 about / data）：
        // 白名单之外的自定义协议在 WebView2 里会被交给系统处理，不该由登录页触发
        _ => false,
    }
}

/// 这个 URL 是不是本 provider 的登录回调（逐项比对 scheme/host/path）。
///
/// host 不区分大小写：自定义协议在 `url` crate 里走 opaque host 解析，不像
/// http(s) 那样被规范化成小写，`office-raccoon://AUTH/callback` 会原样保留。
/// 这里只是**识别**（后端 `raccoon::oauth::parse_callback_code` 才是最终校验，
/// 它有一模一样的口径）；两边都宽一点，避免同一个 URL 在壳与后端得到不同结论。
///
/// **只有小浣熊走回调**：workbuddy 的凭证由后端轮询上游取回，Qoder 走设备授权
/// 轮询（见 `providers::qoder::oauth`），两家都没有自定义协议回调 ——
/// 别把「没有回调」误写成「什么都算回调」。
fn is_login_callback(url: &url::Url, provider: &str) -> bool {
    if provider != "raccoon" {
        return false;
    }
    let host_matches = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .is_some_and(|host| host == RACCOON_CALLBACK_HOST);
    url.scheme() == RACCOON_CALLBACK_SCHEME && host_matches && url.path() == RACCOON_CALLBACK_PATH
}

/// AutoClaw OAuth 的回调：命中时返回**网关自己的**回调地址（把这次导航改道过去）。
///
/// ── 为什么内嵌窗口要拦这一手（issue #11 的兜底）───────────────
/// z.ai 的 redirect_uri 白名单只认官方客户端登记过的那四个 loopback 端口
/// （`18432 / 19654 / 19723 / 53699`，见
/// `providers::autoclaw::callback_server` 的模块头），因此网关交给上游的
/// `navigate_uri` 也必须是那四个端口之一 —— 但那个端口**不一定归我们**：
/// 官方 AutoClaw 客户端一启动就把四个全绑了，回调会被它接走，网关永远等不到
/// 授权码（表现是「登录完成了，界面一直转圈到 5 分钟超时」）。
///
/// 内嵌窗口是我们自己的窗口，可以在**请求发出之前**把这次跳转截下来、改成
/// 访问网关自己的回调路由（授权码由我们换），与谁占着那个端口无关。
/// 抢到端口的那一轮同样会走这里：只是少一次 302 直达，结果一致。
///
/// ── 口径（宽进严出，宁可不拦也别拦错）───────────────────────
/// 只认「http + localhost/127.0.0.1 + 四个登记端口之一 + 两个已知回调路径」，
/// 并且**跳过网关自己的端口** —— 用户完全可以把网关端口设成 18432，那时
/// 回调本来就落在我们身上，再改道就成了自我循环（拦下 → 跳同一个地址 → 拦下）。
fn autoclaw_callback_forward(url: &url::Url) -> Option<String> {
    if url.scheme() != "http" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "localhost" && host != "127.0.0.1" {
        return None;
    }
    let port = url.port_or_known_default()?;
    if port == crate::gateway::proxy_port() {
        return None;
    }
    if !agent2api_server::server::core::providers::autoclaw::callback_server::REGISTERED_CALLBACK_PORTS
        .contains(&port)
    {
        return None;
    }
    let path = url.path();
    if path != "/auth/callback-zai" && path != "/auth/callback-google" {
        return None;
    }
    let query = url
        .query()
        .map(|value| format!("?{value}"))
        .unwrap_or_default();
    Some(format!(
        "http://localhost:{}{path}{query}",
        crate::gateway::proxy_port()
    ))
}

/// 把登录窗口导航到改道后的地址（[`autoclaw_callback_forward`] 的落地点）。
///
/// 用 `navigate` 而不是 `eval("location.href=…")`：前者是壳侧直接下发导航，
/// 不依赖页面上下文还能不能执行脚本（授权页是第三方的，不该假设它允许我们
/// 在里面跑 JS）。窗口在异步任务里可能已经被用户关掉，因此取不到就当无事发生。
fn navigate_login_window(app: &AppHandle, label: &str, target: &str) {
    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    match url::Url::parse(target) {
        Ok(parsed) => {
            if let Err(error) = window.navigate(parsed) {
                eprintln!("[login] AutoClaw 回调改道失败: {error}");
            }
        }
        Err(error) => eprintln!("[login] AutoClaw 回调改道地址无效（{target}）: {error}"),
    }
}

/// 归一化前端传来的 provider id（缺省 workbuddy，只认这五家）。
///
/// ── 为什么 Cline 只列「网页登录」这一条路 ─────────────────────
/// 它的设备授权登录走的就是这条 IPC（壳侧把授权页开出来、后端轮询换令牌），
/// 与 Qoder 的设备授权同形；手工填凭证与导入桌面端登录态都不经过这里
/// （那两条走 `POST /api/accounts`）。
fn normalize_provider(provider: &str) -> Result<&'static str, String> {
    match provider.trim() {
        "" | "workbuddy" => Ok("workbuddy"),
        // WorkBuddy 国际版（2026-10 拆家后是独立 provider）：面板那块
        // 「账号版本」分段按选中项算 provider id，国际版登录走的是同一个
        // 内嵌窗口流程，差别只在后端按它落哪一组账号。**这一条不能少** ——
        // 少了它前端会收到「未知的 provider」而登录按钮直接失败。
        "workbuddy-intl" => Ok("workbuddy-intl"),
        "raccoon" => Ok("raccoon"),
        // Qoder 两个地区（2026-10 拆家后是独立 provider）：设备授权两站同构，
        // 壳侧只透传 provider id，地区由后端按 id 反查（与 ZCode 同款）。
        "qoder" => Ok("qoder"),
        "qoder-intl" => Ok("qoder-intl"),
        "catpaw" => Ok("catpaw"),
        "cline-free" => Ok("cline-free"),
        "cline-pass" => Ok("cline-pass"),
        "autoclaw" | "autoclaw-intl" => Ok("autoclaw-intl"),
        // Accio 两个地区：授权地址由后端适配器拼（PKCE），壳侧只负责开窗口与
        // 轮询 —— 与 workbuddy / Qoder 同一条路。地区由 **provider 本身**决定
        // （两家 provider），壳侧不做归一。
        "accio" => Ok("accio"),
        "accio-cn" => Ok("accio-cn"),
        // ZCode 两个地区：授权地址由**后端**问上游拿（`/oauth/cli/init`），
        // 壳侧只负责开窗口与轮询 —— 与 Qoder / Cline 的设备授权同一条路。
        //
        // 这一家特别注意：它的授权页**没有任何回调落到本机**
        // （见 `providers::zcode::oauth` 的模块头），授权结果由 ZCode 服务端
        // 记录。而本文件的等待机制本来就只看后端 `/api/session/login/wait`
        // 的轮询结果（见 `poll_once` 与 `run_embedded` 的等待循环），不依赖
        // 窗口捕获到回调 —— 因此这条链路天然适配：窗口最后停在 `zcode://`
        // 上打不开，也**不影响**登录判定。
        "zcode" => Ok("zcode"),
        "zcode-intl" => Ok("zcode-intl"),
        // CodeArts（华为云码道）：授权地址由**后端适配器**现拼（PKCE + 回调 URL，
        // 见 `providers::codearts::oauth::authorize_url`），壳侧只负责开窗口与
        // 轮询 —— 与 ZCode 同一条路。它的回调由 portal 拼成
        // `http://127.0.0.1:<网关端口>/oauth/callback` 落回网关自己，所以窗口
        // **必须允许**导航到本机端口（见下面 allowed_hosts 的同一条）。
        "codearts" => Ok("codearts"),
        // Trae（SOLO CN）：授权地址由**后端**问上游 guidance 拼（PKCE + 回调 URL），
        // 壳侧只负责开窗口与轮询 —— 与 ZCode 同一条路。特别地，它的回调落在
        // `http://127.0.0.1:<随机端口>/authorize`（网关自己监听的那台 loopback，
        // 上游把这个地址按正则逐字校验），所以窗口**必须允许**导航到本机端口，
        // 否则用户点完授权、回调请求根本发不出去（见下面 allowed_hosts 的同一条）。
        "trae" => Ok("trae"),
        // Antigravity：授权地址由**后端适配器**本地拼（Google OAuth 授权码，
        // 见 `providers::antigravity::oauth::build_authorize_url`），壳侧只负责
        // 开窗口与轮询 —— 与 CodeArts 同一条路。回调落在
        // `http://localhost:{网关端口}/oauth-callback`（网关自己登记给 Google 的
        // redirect_uri），所以窗口**必须允许**导航到本机端口（见下面
        // allowed_hosts 的同一条），否则用户授权成功后网关永远等不到码。
        "antigravity" => Ok("antigravity"),
        // KukuAI（百度通行证）：授权地址由**后端适配器**拼（客户端同款登录页
        // `passApi/html/loginMerge.html`，见 `providers::kuku::login::build_login_url`），
        // 壳侧负责开窗口（短信自动流程注入脚本；网页登录入口已移除）、并在
        // 登录成功跳转 `success.html` 后读 Cookie 存储交回网关
        // （`POST /api/session/login/kuku/complete`，见 `run_embedded` 的
        // 等待循环与 `submit_kuku_login`）。
        "kuku" => Ok("kuku"),
        // OfficeAce（华为云果办 / OfficeClaw）：授权地址由**后端适配器**现拼
        // （先向云端要 state，见 `providers::officeace::oauth`），壳侧只负责开窗口
        // 与轮询 —— 与 ZCode / CodeArts 同一条路。它的授权码由网关**轮询云端**
        // 取回（云端回调页不往本机跳），所以窗口最后停在云端页上也不影响登录判定。
        "officeace" => Ok("officeace"),
        other => Err(format!("不支持网页登录的提供商：{other}")),
    }
}

/// AutoClaw OAuth 登录：**授权地址已经由前端拿好了**，这里只负责开窗口 + 等待。
///
/// ── 为什么这条与另外五家的形态都不一样 ───────────────────────
/// 那五家的授权地址要么由后端问上游拿（workbuddy / CatPaw / Qoder / Cline），
/// 要么本地拼（小浣熊），壳侧统一做「POST /start → 拿 authUrl → 开窗口」。
/// AutoClaw 国际版多出一个**必须在浏览器里完成的强制风控验证码**（阿里云
/// 浏览器端 SDK，见 `providers::autoclaw::oauth` 的模块头），而主窗口就是
/// 那个浏览器环境 —— 于是顺序被倒过来：**前端先跑验证码、带着参数调网关拿
/// 授权地址，再把地址交给壳开窗口**。
///
/// 因此壳侧这条命令的入参是 `state` + `authUrl`（而不是让壳自己去
/// `/login/start`）—— 那两个值前端已经拿到了，再让壳问一次等于把同一次登录
/// 发起两遍（网关侧会登记两个任务，而回调只认其中一个）。
///
/// ── 回调不需要壳侧识别 ──────────────────────────────────────
/// 授权码由浏览器**直接 302 到本机网关的 loopback 端口**
/// （`navigate_uri` 就是网关自己的地址），因此壳侧既不用捕获自定义协议、
/// 也不用转交回调 —— 与 CatPaw 那条同一形态（见 `core::login::catpaw`）。
/// 这也意味着 `mode = "external"`（系统浏览器）**同样可用**：
/// 回调落在本机网关，与浏览器在哪无关。
pub async fn start_autoclaw_oauth(
    app: &AppHandle,
    state: String,
    auth_url: String,
    mode: &str,
) -> Result<serde_json::Value, String> {
    if state.trim().is_empty() || auth_url.trim().is_empty() {
        return Err("缺少登录状态或授权地址，请重新发起".to_string());
    }
    let use_external = mode == "external";
    let mode_id = if use_external { "external" } else { "embedded" };
    {
        let state_guard = app.state::<crate::state::AppState>();
        let mut guard = state_guard.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: state.clone(),
            edition: "intl".into(),
            mode: mode_id.into(),
            provider: "autoclaw-intl".to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some(mode_id.into()),
            edition: Some("intl".into()),
            provider: Some("autoclaw-intl".to_string()),
        },
    );

    let result = if use_external {
        open_external(app, &auth_url, "AutoClaw 国际版登录页", &state).await
    } else {
        // social_restore 传 false：那项能力只属于 WorkBuddy 的登录页。
        // provider 传 "autoclaw-intl"：它决定白名单（这家不设限，见 allowed_hosts
        // 的 AutoClaw 段 —— 回调落在 loopback，设限会把回调一起拦掉）。
        run_embedded(
            app,
            "autoclaw-intl",
            &auth_url,
            "登录 AutoClaw 国际版账号",
            &state,
            false,
            None,
            false,
        )
        .await
    };
    if result
        .as_ref()
        .map(|value| value.get("ok").and_then(serde_json::Value::as_bool))
        != Ok(Some(true))
    {
        let _ = gateway::call(
            "POST",
            "/api/session/login/cancel",
            Some(&json!({ "state": state })),
        )
        .await;
    }
    clear_active_login(app, &state);
    result
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginState {
    pub active: bool,
    pub mode: Option<String>,
    pub edition: Option<String>,
    /// 进行中登录的 provider（前端据此复位正确的按钮，见 applyLoginState 的调用点）
    pub provider: Option<String>,
}

pub fn current_login(app: &AppHandle) -> Option<ActiveLogin> {
    app.state::<crate::state::AppState>()
        .login
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

/// 把登录进行状态推给渲染层（弹窗按钮据此启用/禁用与显示取消）
fn emit_login_state(app: &AppHandle, state: LoginState) {
    let _ = app.emit("login:state", state);
}

/// 一次 `/wait` 的结果分类。
///
/// ── 为什么要把「任务失败」与「本地网关暂时读不到」分开 ────────
/// 原来的实现把两者都当成「打印一行、继续轮询」，于是**任何**失败都要等到
/// 5 分钟超时才反馈给用户，而且文案是「等待超时」——用户看不到真实原因
/// （小浣熊那边尤其明显：state 校验失败、授权码已失效都会被这条超时盖掉）。
/// 现在：任务已落定（done + error）就立刻结束等待并透出真实文案；
/// 只有传输层错误（本地网关瞬时不可达）才继续重试 —— 那类错误下一拍自愈，
/// 提前放弃反而会把一次正常的登录掐断。
enum PollOutcome {
    /// `{pending:true}`
    Pending,
    /// `{done:true, …}` —— 携带任务载荷（`{account, warning…}`，登录收尾
    /// 时透传给前端，例如 KukuAI 的「凭证已落库但未通过上游复核」警告）。
    Done(serde_json::Value),
    /// `{done:true, error:…}` —— 终态，文案原样给用户
    Failed(String),
    /// 读不到结果：本地网关的传输层错误（连接失败 / 非 2xx 信封）。
    ///
    /// 仍然带文案，因为其中一类**不是**「稍后会自愈」：用户取消或任务过期时
    /// 后端把任务从表里删掉，`/wait` 回 404「登录任务不存在或已过期」。
    /// 那种情况必须结束等待（见 `task_gone`），否则关掉弹窗后这个循环会一直
    /// 转到 5 分钟超时 —— 表现为「取消后要等 5 分钟按钮才恢复」。
    Unreachable(String),
}

/// 读一次后端登录状态。
async fn poll_once(state: &str) -> PollOutcome {
    let path = format!("/api/session/login/wait?state={state}");
    let value = match gateway::call("GET", &path, None).await {
        Ok(value) => value,
        Err(error) => return PollOutcome::Unreachable(error),
    };
    if value.get("pending").and_then(serde_json::Value::as_bool) == Some(true) {
        return PollOutcome::Pending;
    }
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return PollOutcome::Failed(error.to_string());
    }
    PollOutcome::Done(value)
}

/// 判断一条等待期错误是不是「任务已被后端清理」（取消 / 过期）。
/// 这两种情况下后端把任务从表里删掉了，`/wait` 回 404「登录任务不存在或已过期」。
fn task_gone(message: &str) -> bool {
    message.contains("登录任务不存在") || message.contains("已过期")
}

/// 通知后端中止轮询；用于用户关窗/点取消
pub async fn cancel(app: &AppHandle) -> Result<(), String> {
    let active = current_login(app);
    if let Some(active) = active {
        if let Some(window) = app.get_webview_window(&login_window_label(&active.state)) {
            let _ = window.destroy();
        }
        clear_active_login(app, &active.state);
        let _ = gateway::call(
            "POST",
            "/api/session/login/cancel",
            Some(&json!({ "state": active.state })),
        )
        .await;
    }
    Ok(())
}

/// 发起一次登录。阻塞到登录完成、失败、取消或超时。
///
/// `provider` 缺省（空串）按 workbuddy 处理 —— 老版本界面不会传它，
/// 那条链的行为必须逐字保持。
///
/// `social_restore`：是否恢复 WorkBuddy 登录页的 Google / GitHub / X 入口
/// （见 [`social_restore_script`]）。界面默认不勾选，故缺省 false。
pub async fn start(
    app: &AppHandle,
    edition: String,
    mode: String,
    provider: String,
    social_restore: bool,
) -> Result<serde_json::Value, String> {
    if current_login(app).is_some() {
        return Err("已有登录流程在进行，请先完成当前登录或等待超时".to_string());
    }
    let provider = normalize_provider(&provider)?;

    // 小浣熊：授权地址由**后端适配器**生成（静态地址 + 本地生成的 state），
    // 所以请求体里只带 provider，不带 edition（那是 workbuddy 的端点维度）。
    if provider == "raccoon" {
        return start_raccoon(app).await;
    }
    // KukuAI：百度通行证官方登录页（适配器生成授权地址），登录成功后由壳侧
    // 读 Cookie 存储交回网关（见 `submit_kuku_login`）。
    if provider == "kuku" {
        return start_kuku(app, &mode).await;
    }
    // CatPaw：授权地址要问一次上游 `login-config`，且 token 由上游**推**到本网关
    // 的 loopback 回调上（见 core::login::catpaw）。同样没有 edition 维度。
    if provider == "catpaw" {
        return start_catpaw(app, &mode).await;
    }

    // Qoder 的设备授权两站同构（见 core::login::qoder），`cn` 原样透传给后端 ——
    // 由它决定授权页与轮询地址打哪一站。这里不再拦截国内版。
    //
    // ── Cline 不再走这个形参传池（拆分后删掉的一段）────────────
    // 早先 `cline` 借用 `edition` 这个位置传额度池（`pass` / `free`），因为那时
    // 池是**账号的属性**、得由界面选一次。现在两个池是**两个 provider**
    // （`cline-free` / `cline-pass`），池已经在 provider id 里，没有第二个旋钮 ——
    // 这里再算一遍 edition 就是多余的一处状态（还会与 provider 冲突时说不清谁算数）。
    let edition_id = match provider {
        // Qoder 两家 provider 各自固定地区（界面上是两张卡片，没有地区下拉），
        // 归属已经在 provider id 里 —— 与 ZCode 同款，不再读 edition 形参。
        "qoder" => "cn",
        "qoder-intl" => "intl",
        // ZCode 的两个地区由 **provider 本身**决定（界面上是两张卡片，
        // 没有地区下拉，因此面板不会传 `edition`）。不看 `edition` 形参的
        // 后果只是标题里的「国内版/国际版」四个字，但既然信息已经在
        // provider id 里，就不该再去读一个恒为空的形参。
        "zcode" => "cn",
        "zcode-intl" => "intl",
        // WorkBuddy 国际版同理（拆家后它是独立 provider）：归属已经在 provider id
        // 里，这里只把它翻成标题/日志用的地区名。面板仍会同时传 `edition`
        // （同一块里的分段控件），两者一致时无所谓，不一致时**以 provider 为准**
        // —— 与后端 `login_start` 的口径相同（见 api::session 的说明）。
        "workbuddy-intl" => "intl",
        _ => {
            if edition == "intl" { "intl" } else { "cn" }
        }
    };
    let edition_label = if edition_id == "intl" { "国际版" } else { "国内版" };
    // 系统浏览器模式对 workbuddy / qoder / cline 都开放：三家的判定都在后端
    // （轮询上游 auth/token、轮询设备令牌、轮询设备授权），浏览器在哪登录都行。
    // 小浣熊**只有内嵌窗口**能收回调（自定义协议深链要靠系统注册，见模块头），
    // 给它这个选项只会制造一个永远等不到回调的卡死。
    let use_external = mode == "external" && provider != "raccoon";

    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "edition": edition_id, "provider": provider })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: edition_id.to_string(),
            mode: if use_external { "external".into() } else { "embedded".into() },
            provider: provider.to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some(if use_external { "external".into() } else { "embedded".into() }),
            edition: Some(edition_id.to_string()),
            provider: Some(provider.to_string()),
        },
    );

    // 窗口标题里的品牌名。缺省 WorkBuddy 是历史契约（老客户端只走那一家），
    // 其余家各自点名 —— 少写一家只会让标题显示成「登录 WorkBuddy 国内版账号」
    // 而实际打开的是别家的页面，用户第一眼就会以为是点错了按钮。
    let provider_label = match provider {
        // Qoder 两家品牌名自带地区（两张卡片、无地区下拉），标题不拼后缀
        "qoder" => "Qoder 中国版",
        "qoder-intl" => "Qoder 国际版",
        "cline-free" => "Cline Free",
        "cline-pass" => "Cline Pass",
        "catpaw" => "CatPaw",
        "raccoon" => "小浣熊",
        // ZCode 两家各自点名，且品牌名里**已经带了地区** —— 因此下面拼标题时
        // 要跳过 edition 后缀，否则会得到「登录 ZCode 国内版 国内版账号」
        "zcode" => "ZCode 国内版",
        "zcode-intl" => "ZCode 国际版",
        // CodeArts 只有一家（region 固定在 cn-north-4 且必须与 token 签发地
        // 一致，不是用户可选项，见 `providers::codearts` 的模块头），
        // 品牌名里不需要地区
        "codearts" => "CodeArts",
        // Trae 只有一家（国内 SOLO 通道；国际版是另一套协议、另立 provider id），
        // 品牌名里不需要地区
        "trae" => "Trae",
        // Antigravity（Google 的 AI IDE）只有一家，没有地区之分
        // （本仓 `providers::antigravity` 的模块头：端点全球统一）
        "antigravity" => "Antigravity",
        // OfficeAce 只有一家（自助 OAuth 一条链，没有地区/版本概念），品牌名里
        // 不需要地区
        "officeace" => "OfficeAce 果办",
        _ => "WorkBuddy",
    };
    // 窗口标题：Cline 两家的池、ZCode 两家的地区、CodeArts / Trae / OfficeAce
    // 的单一家都已经在品牌名里，不再拼 edition 后缀（否则会出现「登录 Cline Free
    // 国内版账号」这种说不通的标题）
    let title = if matches!(
        provider,
        "cline-free" | "cline-pass" | "zcode" | "zcode-intl" | "codearts" | "trae" | "qoder"
        | "qoder-intl" | "antigravity" | "officeace"
    ) {
        format!("登录 {provider_label} 账号")
    } else {
        format!("登录 {provider_label} {edition_label}账号")
    };
    // 第三方入口恢复只对**国际版 WorkBuddy** 有意义，在这里就把条件算完整，
    // 传给 run_embedded 的就是「这次登录要不要恢复入口」的最终答案：
    //   ① 只有 workbuddy 的登录链有这个概念（小浣熊 / CatPaw / Qoder 另有分流）；
    //   ② 只有国际版登录页被上游隐藏了入口 —— 国内版登录页是微信 / 手机号 /
    //      邮箱 / SSO，既没有 Google / GitHub 按钮，也没有那段隐藏样式（实测），
    //      对它放行 google.com 之类的域名属于没有必要的放宽。
    // 判据取**地区**而不是某个 id：拆家后国际版是 `workbuddy-intl`，
    // 写单 id 会让它的登录页不再恢复第三方入口（症状是 Google / GitHub 按钮
    // 点了白屏 —— 与国内版没有那两个按钮是两回事）。
    let intl_workbuddy = {
        use agent2api_server::server::core::providers::workbuddy::Region;
        Region::from_provider_id(provider).is_some_and(|region| region != Region::Cn)
    };
    let social_restore = social_restore && intl_workbuddy;
    let result = if use_external {
        open_external(app, &auth_url, edition_label, &login_state).await
    } else {
        run_embedded(app, provider, &auth_url, &title, &login_state, social_restore, None, false).await
    };

    if result.as_ref().map(|value| value.get("ok").and_then(serde_json::Value::as_bool)) != Ok(Some(true)) {
        let _ = gateway::call("POST", "/api/session/login/cancel", Some(&json!({ "state": login_state }))).await;
    }
    clear_active_login(app, &login_state);
    result
}

/// 小浣熊网页登录：内嵌窗口 + 回调由窗口自己捕获提交（见模块头）。
///
/// 只给内嵌窗口一种方式，理由见模块头（系统浏览器模式下自定义协议深链
/// 需要系统注册 `office-raccoon://`，那是官方客户端装的，装了才有）。
async fn start_raccoon(app: &AppHandle) -> Result<serde_json::Value, String> {
    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "provider": "raccoon" })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: String::new(),
            mode: "embedded".into(),
            provider: "raccoon".to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some("embedded".into()),
            edition: None,
            provider: Some("raccoon".to_string()),
        },
    );

    // social_restore 传 false：第三方入口恢复只针对 WorkBuddy 的登录页
    // （`run_embedded` 里也只对 provider == "workbuddy" 注入脚本），小浣熊
    // 没有这个开关概念，直传 false 表明「不参与这项能力」。
    let result = run_embedded(app, "raccoon", &auth_url, "登录小浣熊账号", &login_state, false, None, false).await;
    if result.as_ref().map(|value| value.get("ok").and_then(serde_json::Value::as_bool)) != Ok(Some(true)) {
        let _ = gateway::call("POST", "/api/session/login/cancel", Some(&json!({ "state": login_state }))).await;
    }
    clear_active_login(app, &login_state);
    result
}

/// KukuAI 网页登录：**百度通行证官方登录页 + 壳侧读 Cookie 存储交回**。
///
/// 与其它家网页登录的差别（见 `providers/kuku/login.rs` 的模块头）：
/// passport 登录成功后的跳转不携带授权码（凭证在 `.baidu.com` 域 Cookie 里），
/// 因此不走「导航拦截 → 提交回调 URL」那条链，而是由壳侧在窗口跳转
/// `success.html` 后读 WebView2 的 Cookie 存储（`BDUSS` 是 HttpOnly，
/// 页面脚本读不到）POST 网关的 `POST /api/session/login/kuku/complete`
/// （见 `run_embedded` 的等待循环与 `submit_kuku_login`）。
///
/// 前端已不提供网页登录入口（短信流程的隐藏窗口覆盖了同一页面），此函数
/// 作为防御性保留（`start` 命令仍可能被老版本调用）。
async fn start_kuku(app: &AppHandle, mode: &str) -> Result<serde_json::Value, String> {
    // 登录态由壳侧读登录窗口的 Cookie 存储交回（见 `submit_kuku_login`）——
    // 系统浏览器里壳读不到，登录完成网关等不到 Cookie。因此 external 模式
    // 直接拒绝，不静默退回内嵌（用户点了 external 却弹出内嵌窗口，是
    // 「按钮与行为对不上」）。
    if mode == "external" {
        return Err(
            "KukuAI 的登录依赖壳侧读取登录窗口的 Cookie，只支持内嵌窗口；\
             请用「内嵌窗口」发起，或改用「粘贴 Cookie」添加账号"
                .to_string(),
        );
    }
    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "provider": "kuku" })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: String::new(),
            mode: mode.into(),
            provider: "kuku".to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some(mode.into()),
            edition: None,
            provider: Some("kuku".to_string()),
        },
    );

    let result = run_embedded(app, "kuku", &auth_url, "登录 KukuAI 账号", &login_state, false, None, false).await;
    if result.as_ref().map(|value| value.get("ok").and_then(serde_json::Value::as_bool)) != Ok(Some(true)) {
        let _ = gateway::call("POST", "/api/session/login/cancel", Some(&json!({ "state": login_state }))).await;
    }
    clear_active_login(app, &login_state);
    result
}

/// CatPaw 网页登录：**passport 登录页 + 上游回调打到网关自己的 loopback 端口**。
///
/// 与另两家的差别（见 `core::login::catpaw` 的模块头）：token 是**上游推给
/// 我们**的 —— 美团 passport 的 `login-callback` 页面把 `{token, state}` 表单
/// POST 到授权 URL 里给的 `redirect`，而那个地址就是本网关的
/// `127.0.0.1:<port>/api/session/login/catpaw-callback`。因此这里只需要
/// 「开窗口 → 轮询 /wait」，不需要壳侧做任何回调识别或转交。
///
/// `mode`：内嵌窗口与系统浏览器都支持（回调打回本机网关，与浏览器在哪无关），
/// 与 Qoder 同理；小浣熊那种「只有内嵌能收回调」的约束在这家不成立。
async fn start_catpaw(
    app: &AppHandle,
    mode: &str,
) -> Result<serde_json::Value, String> {
    let started = gateway::call(
        "POST",
        "/api/session/login/start",
        Some(&json!({ "provider": "catpaw" })),
    )
    .await?;
    let login_state = started
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录状态")?
        .to_string();
    let auth_url = started
        .get("authUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or("后端未返回登录链接，请检查网络")?
        .to_string();

    let use_external = mode == "external";
    let mode_id = if use_external { "external" } else { "embedded" };
    {
        let state = app.state::<crate::state::AppState>();
        let mut guard = state.login.lock().map_err(|_| "登录状态锁不可用")?;
        *guard = Some(ActiveLogin {
            state: login_state.clone(),
            edition: String::new(),
            mode: mode_id.into(),
            provider: "catpaw".to_string(),
        });
    }
    emit_login_state(
        app,
        LoginState {
            active: true,
            mode: Some(mode_id.into()),
            edition: None,
            provider: Some("catpaw".to_string()),
        },
    );

    let result = if use_external {
        // "CatPaw 官方登录页" 是给超时文案用的：这家没有「国际版 / 国内版」这一级，
        // 传空串会得到「已完成登录，但等待超时」这种缺主语的句子。
        open_external(app, &auth_url, "CatPaw 官方登录页", &login_state).await
    } else {
        // social_restore 传 false：与小浣熊同理，这项能力只属于 WorkBuddy 的登录页。
        run_embedded(app, "catpaw", &auth_url, "登录 CatPaw 账号", &login_state, false, None, false).await
    };
    if result.as_ref().map(|value| value.get("ok").and_then(serde_json::Value::as_bool)) != Ok(Some(true)) {
        let _ = gateway::call("POST", "/api/session/login/cancel", Some(&json!({ "state": login_state }))).await;
    }
    clear_active_login(app, &login_state);
    result
}

/// 只结束本次登录，旧窗口的关闭事件不能清掉后来发起的登录。
fn clear_active_login(app: &AppHandle, expected_state: &str) {
    let state = app.state::<crate::state::AppState>();
    if let Ok(mut guard) = state.login.lock() {
        if guard.as_ref().is_some_and(|active| active.state == expected_state) {
            *guard = None;
            emit_login_state(app, LoginState { active: false, mode: None, edition: None, provider: None });
        }
    };
}

fn login_is_current(app: &AppHandle, expected_state: &str) -> bool {
    current_login(app).is_some_and(|active| active.state == expected_state)
}

fn login_window_label(login_state: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("login-{:x}", Sha256::digest(login_state.as_bytes()))
}

/// 系统浏览器模式：浏览器与登录页共享登录态，完成后仍由后端轮询判定。
/// 用户关掉浏览器不影响等待（登录可能已完成）。
async fn open_external(
    app: &AppHandle,
    auth_url: &str,
    edition_label: &str,
    login_state: &str,
) -> Result<serde_json::Value, String> {
    if std::env::var("WORKBUDDY_SKIP_OPEN_BROWSER").as_deref() != Ok("1") {
        open_in_browser(auth_url)?;
    }
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        // 已被取消（关弹窗/点取消）：停止轮询，不当作错误
        if !login_is_current(app, login_state) {
            return Ok(json!({ "ok": false, "canceled": true }));
        }
        match poll_once(login_state).await {
            PollOutcome::Pending => continue,
            PollOutcome::Done(payload) => {
                return Ok(json!({ "ok": true, "external": true, "payload": payload }))
            }
            PollOutcome::Failed(error) => {
                // 任务已落定失败（后端写进任务的终态错误）：立刻透出真实原因，
                // 不再把它盖成「等待超时」——小浣熊那条链上的「授权码已失效 /
                // state 校验失败」都是这一类。
                return Err(error);
            }
            PollOutcome::Unreachable(error) => {
                // 任务被后端清理（已取消 / 已过期，`/wait` 回 404）时直接结束，
                // 不必等满 5 分钟；其余传输层错误下一拍可能自愈，继续等。
                if task_gone(&error) {
                    return Ok(json!({ "ok": false, "canceled": true }));
                }
                eprintln!("[login] 读取登录进度失败（继续等待）: {error}");
                continue;
            }
        }
    }
    Err(format!("已打开系统浏览器完成{edition_label}登录，但等待超时（5 分钟），请重试"))
}

/// 内嵌 WebView 模式：建独立窗口加载登录页，同时轮询后端。
///
/// `provider` 决定白名单与回调识别（小浣熊要捕获 `office-raccoon://` 深链）；
/// `title` 由调用方给出（两家窗口标题不同）。
///
/// `social_restore` 是**已经算好的结论**（调用方已按 provider 与版本收窄，见 `start`）：
/// 为 true 时注入 [`social_restore_script`] 并放行第三方域名 —— 两项必须成对生效：
/// 只注入不放行，点了 Google 登录会因导航被拦而白屏。
///
/// `kuku_phone`：**KukuAI 短信自动流程**的手机号（`Some` 时注入
/// [`kuku_sms_script`]：脚本自动填号发码、等验证码、自动登录 —— 验证码由
/// `submit_kuku_sms_code` 经 `window.eval` 写入页面变量；登录态由壳侧在
/// 等待循环里读 Cookie 存储交回，见 [`submit_kuku_login`]）。
/// `None` = 网页登录（用户手动操作，交回同样走壳侧读 Cookie）。
///
/// `start_hidden`：窗口**以隐藏状态创建**（KukuAI 短信流程用：整个登录
/// 由脚本后台完成，用户不该看到网页 —— 只有遇到图形验证码等需要人工介入
/// 的情况才由调用方把窗口亮出来，见 `start_kuku_sms` 的监视任务）。
async fn run_embedded(
    app: &AppHandle,
    provider: &'static str,
    auth_url: &str,
    title: &str,
    login_state: &str,
    social_restore: bool,
    kuku_phone: Option<&str>,
    start_hidden: bool,
) -> Result<serde_json::Value, String> {
    let window_label = login_window_label(login_state);
    // ── 每次登录一个独立环境（修复：第二次添加账号会看到上一个账号的登录态）──
    // 每次登录用**全新**的临时数据目录：WebView2 的 Cookie / localStorage / 缓存
    // 都按数据目录存放，共用默认目录就等于共用登录态 —— 添加第二个账号时官方
    // 登录页会把上一个账号直接放行过去，用户根本没有机会换成另一个账号。
    // 目录在窗口关闭后删除（见 login_profile.rs）。
    //
    // **刻意不开隐私模式**（`incognito(true)`）：那个开关会让 Google / GitHub 之类
    // 的身份提供方判为「不支持登录的浏览器」而拒绝登录（多次看到点登录没反应），
    // 而独立数据目录已经提供了本功能真正需要的隔离 —— 每个账号一份干净的
    // Cookie 存储，与前一个账号互不影响。
    let profile = LoginProfile::new()?;
    let url = url::Url::parse(auth_url).map_err(|error| format!("登录链接无效: {error}"))?;

    // ── 回调只处理一次 ────────────────────────────────────────
    // 同一个回调 URL 可能从多个来源到达（WebView2 对「导航到自定义协议」在
    // 某些路径上会先触发 NavigationStarting 再派生新窗口请求，而放行后系统
    // 处理失败还可能再来一次）。授权码是**一次性**的：第二次提交只会从上游
    // 换来 200035「已失效」，把一次成功登录变成一次失败。这个标志与源项目
    // Electron 版的 `callbackSeen` 同一用途。
    let callback_seen = Arc::new(AtomicBool::new(false));
    let login_state_owned = login_state.to_string();
    // KukuAI 登录态只交回一次（主站登录的 Cookie 检测每拍触发，
    // 成功后立刻交回；重复 POST complete 虽然幂等，但没有必要多发）。
    let kuku_submitted = Arc::new(AtomicBool::new(false));
    // KukuAI 主站方案的 Cookie 检测节拍：
    //   - **导航事件**（on_navigation）计数变化 → 下一拍立即读 cookie
    //     （用户登录时必然经过导航：跳 passport、回跳主站）；
    //   - 每 4 拍（8 秒）兜底读一次（用户登录后长时间不导航也不漏）；
    //   - 首次发现 BDUSS 后再等 4 秒才交回：登录瞬间影子 cookie
    //     （BDUSS_BFESS 等）可能尚未刷新完，立刻交回会带着旧影子值被判
    //     「未登录」（userreport errno=-6，2026-10-07 真实踩过）。
    let mut kuku_check_tick: u32 = 0;
    let kuku_nav_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut kuku_last_nav: u64 = 0;
    let nav_counter_for_check = kuku_nav_counter.clone();
    let nav_counter_for_nav = kuku_nav_counter.clone();
    let mut kuku_bduss_seen_at: Option<tokio::time::Instant> = None;
    // 交回失败后的退避（30 秒内不重复提交）：后端那次复核失败多为上游
    // 临时风控限流（同一凭证几分钟后即恢复，2026-10-07 实测对照），
    // 密集重试只会延长风控窗口。
    let mut kuku_retry_block_until: Option<tokio::time::Instant> = None;

    // 导航拦截与弹窗拦截**共用同一个标志**：两种入口收到的可能是同一个回调
    // （页面先改 location、再被 WebView2 派生出一条弹窗请求），分开各一个标志
    // 就会提交两次，第二次拿到的必然是「授权码已失效」。
    let seen_for_nav = callback_seen.clone();
    let state_for_nav = login_state_owned.clone();
    let seen_for_window = callback_seen.clone();
    let state_for_window = login_state_owned.clone();
    // 回调改道用（见 `autoclaw_callback_forward`）：导航拦截的闭包拿不到窗口
    // 本身（窗口还在构建中），只能带着 app 与 label，等真要改道时再取。
    let app_for_forward = app.clone();
    let label_for_forward = window_label.clone();
    let app_for_window = app.clone();
    let label_for_window = window_label.clone();

    let window = WebviewWindowBuilder::new(app, &window_label, WebviewUrl::External(url))
        .data_directory(profile.path().to_path_buf())
        // 免去密码/地址的自动填充建议：这个窗口只用来过一次登录，填充弹层会
        // 盖住授权码。老运行时上没有 Settings4 接口，wry 会跳过它。
        .general_autofill_enabled(false)
        .title(title.to_string())
        .inner_size(1100.0, 820.0)
        .min_inner_size(760.0, 560.0)
        .visible(!start_hidden)
        .center();

    // 恢复第三方登录入口：登录页按上游开关把 Google / GitHub / X 隐藏了，
    // 注入脚本持续摘掉那段样式（理由与边界见 social_restore_script）。
    //
    // 用 for_all_frames：目标样式在**内层 iframe** 的 head 里，只注入主 frame
    // 够不着它。脚本自身再按 hostname 限一次，避免跳去 accounts.google.com
    // 之后还在那边空转。
    //
    // KukuAI 走自己的注入脚本（仅短信自动流程需要）：
    // `kuku_phone` 非空时注入「短信自动流程」脚本（自动填号发码、等验证码、
    // 自动登录，见 `kuku_sms_script`）；`None`（网页登录）不需要脚本 ——
    // 用户手动操作，登录态由壳侧读 Cookie 存储交回（见等待循环的 kuku
    // 分支与 `submit_kuku_login`）；登录后的「会话激活」在后端做
    // （`providers/kuku/engine.rs`）。与 social_restore 互斥，按 provider 二选一。
    let window = if provider == "kuku" {
        match kuku_phone {
            Some(phone) => window.initialization_script(kuku_sms_script(phone)),
            None => window,
        }
    } else if social_restore {
        window.initialization_script_for_all_frames(social_restore_script())
    } else {
        window
    };

    let window = window
        // 登录页只允许在上游白名单域名之间跳转：登录页会经过 SSO 中转，
        // 若页面被注入任意跳转，凭据可能被带到第三方站点。
        //
        // ── 为什么回调必须先于白名单判定 ────────────────────────
        // `host_allowed` 对自定义协议一律 false，先判白名单就会把回调当成
        // 「非法导航」拦掉 —— 拦掉的动作与「捕获回调」在 WebView2 眼里完全相同，
        // 症状是用户明明登录成功、网关却永远等不到 code。
        //
        // ── AutoClaw 的回调为什么先「改道」（本次修正）────────────
        // 它的回调落在 z.ai 登记的那四个端口上，那几个端口可能被官方客户端
        // 占着（回调会被它接走）。这里在请求发出前把导航改到网关自己的回调
        // 路由 —— 理由与口径见 `autoclaw_callback_forward`。
        .on_navigation(move |url| {
            if let Some(target) = autoclaw_callback_forward(url) {
                let app = app_for_forward.clone();
                let label = label_for_forward.clone();
                tauri::async_runtime::spawn(async move {
                    navigate_login_window(&app, &label, &target);
                });
                return false;
            }
            if is_login_callback(url, provider) {
                // 返回 false 即阻止这次导航（否则 WebView2 会把它交给系统：
                // 本机没注册 office-raccoon:// 时是一个错误页）。
                // 回调处理是网络动作，而本回调是同步的，因此 spawn 出去做。
                if !seen_for_nav.swap(true, Ordering::SeqCst) {
                    let login_state = state_for_nav.clone();
                    let callback_url = url.as_str().to_string();
                    tauri::async_runtime::spawn(async move {
                        submit_callback(&login_state, &callback_url).await;
                    });
                }
                return false;
            }
            // KukuAI 主站登录：每次导航（含跳 passport、登录后回跳主站）
            // 计数 +1，等待循环据此立即读一次 Cookie 存储做登录检测。
            if provider == "kuku" {
                nav_counter_for_nav.fetch_add(1, Ordering::SeqCst);
            }
            host_allowed(url, provider, social_restore)
        })
        // 弹出窗口（window.open）也是回调的可能入口：有的登录页在拿到授权码后会用
        // `window.open('office-raccoon://…')` 而不是直接改 location —— 那样它就只
        // 走 NewWindowRequested，导航拦截根本看不到，用户会看到「登录成功了但网关
        // 没反应」。这里同样**先判回调再统一拒绝**：
        //   - 是我们的回调 → 拦下并提交（去重标志共用，见上）；
        //   - 其余一律 Deny —— 这与不注册本回调时的默认行为**完全一致**
        //     （wry 在没有 handler 时对每个 NewWindowRequested 都 SetHandled(true)），
        //     所以这不是「放开弹窗」，只是把回调那一种从被静默丢弃变成被接住。
        //
        // AutoClaw 的改道同样先判（它走的是顶层 302，正常不该落到这里；但授权页
        // 若改用 window.open 打开回调，没有这一支就会「登录成功却毫无反应」）。
        //
        // KukuAI 主站例外：它的「登录」按钮可能以弹窗打开 passport 登录页。
        // 一律 Deny 会让用户点登录毫无反应 —— 这里**放行**（弹窗与主窗口
        // 同属一个 WebView2 数据目录、共享 Cookie 存储，弹窗里登录成功后
        // 等待循环照样能从 Cookie 读到 BDUSS）。
        .on_new_window(move |url, _features| {
            if provider == "kuku" {
                return tauri::webview::NewWindowResponse::Allow;
            }
            if let Some(target) = autoclaw_callback_forward(&url) {
                let app = app_for_window.clone();
                let label = label_for_window.clone();
                tauri::async_runtime::spawn(async move {
                    navigate_login_window(&app, &label, &target);
                });
                return tauri::webview::NewWindowResponse::Deny;
            }
            if is_login_callback(&url, provider) && !seen_for_window.swap(true, Ordering::SeqCst) {
                let login_state = state_for_window.clone();
                let callback_url = url.as_str().to_string();
                tauri::async_runtime::spawn(async move {
                    submit_callback(&login_state, &callback_url).await;
                });
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .build()
        .map_err(|error| format!("打开登录窗口失败: {error}"))?;

    // 关窗即视为放弃登录：通知后端中止轮询，并让等待循环退出
    let handle = app.clone();
    let state_for_close = login_state.to_string();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            if login_is_current(&handle, &state_for_close) {
                clear_active_login(&handle, &state_for_close);
                let payload = json!({ "state": state_for_close });
                tauri::async_runtime::spawn(async move {
                    let _ = gateway::call("POST", "/api/session/login/cancel", Some(&payload)).await;
                });
            }
        }
    });

    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    let outcome = loop {
        if tokio::time::Instant::now() >= deadline {
            break Err("网页登录等待超时（5 分钟），请重试".to_string());
        }
        tokio::time::sleep(POLL_INTERVAL).await;

        // 窗口被关掉（或已取消）：结束等待
        if app.get_webview_window(&window_label).is_none() || !login_is_current(app, login_state) {
            break Ok(json!({ "ok": false, "canceled": true }));
        }
        // KukuAI（主站登录方案，见 `providers/kuku/login.rs` 的模块头）：
        // 用户在内嵌窗口的 `kuku.baidu.com` 里正常登录，壳侧以 **Cookie 存储
        // 里出现 BDUSS** 作为成功判据（读 cookie 走独立线程 + 超时，见
        // `read_window_cookies`；Windows 上同步调 cookies() 会死锁）。
        // 触发时机：导航事件（登录必然经过导航：跳 passport、回跳主站）
        // 之后立即读一次；另有每 4 拍（8 秒）兜底，防止「登录后长时间
        // 不导航」漏掉。首次发现 BDUSS 后再等 4 秒交回（等影子 cookie
        // BDUSS_BFESS 刷新完，见上面 `kuku_bduss_seen_at` 的说明）。
        //
        // 交回后的「会话激活」（换发 genflowpro 作用域 STOKEN）在后端做
        // （`providers/kuku/engine.rs`）—— 那一步要调官方客户端引擎算签名，
        // 浏览器页面里做不了，壳侧不再做任何等待门。
        if provider == "kuku" {
            kuku_check_tick += 1;
            let nav_count = nav_counter_for_check.load(Ordering::SeqCst);
            let navigated = nav_count != kuku_last_nav;
            if navigated {
                kuku_last_nav = nav_count;
            }
            let quick_url_hit = window
                .url()
                .map(|current| current.to_string().contains("/passApi/html/success.html"))
                .unwrap_or(false);
            let mut should_submit = quick_url_hit;
            if !kuku_submitted.load(Ordering::SeqCst)
                && (navigated || kuku_bduss_seen_at.is_some() || kuku_check_tick % 4 == 0)
            {
                match read_window_cookies(&window).await {
                    Ok(cookies) => {
                        let (header, has_bduss) = kuku_cookie_header(&cookies);
                        eprintln!(
                            "[login] KukuAI Cookie 检测：共 {} 条，BDUSS {}（头长 {}）",
                            cookies.len(),
                            if has_bduss { "已出现" } else { "未出现" },
                            header.len()
                        );
                        if has_bduss {
                            let is_first = kuku_bduss_seen_at.is_none();
                            let first_seen = *kuku_bduss_seen_at
                                .get_or_insert_with(tokio::time::Instant::now);
                            if is_first {
                                kuku_log_bduss_candidates(&cookies);
                            }
                            let stable = first_seen.elapsed() >= Duration::from_secs(4);
                            if stable {
                                // 用**刚读到的最新** cookie 交回（等稳定期间
                                // 影子 cookie 可能已刷新）
                                should_submit = true;
                            } else {
                                eprintln!(
                                    "[login] KukuAI 已发现 BDUSS，等 4 秒让会话稳定后再交回…"
                                );
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("[login] KukuAI 读取窗口 Cookie 失败（下轮重试）: {error}");
                    }
                }
            }
            if should_submit && !kuku_submitted.load(Ordering::SeqCst) {
                let blocked = kuku_retry_block_until
                    .map(|until| tokio::time::Instant::now() < until)
                    .unwrap_or(false);
                if blocked {
                    eprintln!("[login] KukuAI 交回退避中（30 秒），等下一轮再试");
                } else {
                    kuku_submitted.store(true, Ordering::SeqCst);
                    if let Err(error) = submit_kuku_login(&window, login_state).await {
                        eprintln!("[login] 交回 KukuAI 登录态失败（下轮重试）: {error}");
                        kuku_submitted.store(false, Ordering::SeqCst);
                        kuku_retry_block_until = Some(
                            tokio::time::Instant::now() + Duration::from_secs(30),
                        );
                    } else {
                        kuku_bduss_seen_at = None;
                    }
                }
            }
        }
        match poll_once(login_state).await {
            PollOutcome::Pending => continue,
            // 任务载荷（`{account, warning…}`）随成功结果透传：前端据此
            // 在「账号已添加」之后补一条警告（例如 KukuAI 的凭证未通过
            // 上游复核 —— 账号未开通 KukuAI 时就是这条路）。
            PollOutcome::Done(payload) => break Ok(json!({ "ok": true, "payload": payload })),
            // 任务已落定失败（state 校验失败 / 授权码已失效 / 换取凭证失败）：
            // 立刻透出真实文案。继续轮询只会把它盖成 5 分钟后的「等待超时」，
            // 用户看不到任何可行动的原因。
            PollOutcome::Failed(error) => break Err(error),
            PollOutcome::Unreachable(error) => {
                // 任务已被后端清理（用户取消 / 过期）→ 与关窗同一种结局
                if task_gone(&error) {
                    break Ok(json!({ "ok": false, "canceled": true }));
                }
                eprintln!("[login] 读取登录进度失败（继续等待）: {error}");
                continue;
            }
        }
    };

    let _ = window.destroy();
    drop(window);
    drop(profile);
    outcome
}

/// 把窗口捕获到的回调 URL 交给后端换凭证。
///
/// 失败只打日志：真正的错误文案由后端写进登录任务，随下一次 `/wait` 返回，
/// 前端与等待循环都从那里读（同一个出口，不必在这里造第二份文案）。
/// 等待循环每 2 秒看一次 `/wait`，回调整体是一次本地 HTTP 调用（毫秒级），
/// 因此不需要额外唤醒机制。
async fn submit_callback(login_state: &str, callback_url: &str) {
    let payload = json!({ "state": login_state, "callbackUrl": callback_url });
    match gateway::call("POST", "/api/session/login/callback", Some(&payload)).await {
        Ok(_) => eprintln!("[login] 已捕获登录回调并提交给网关"),
        Err(error) => eprintln!("[login] 提交登录回调失败: {error}"),
    }
}

/// 读登录窗口的 WebView2 Cookie 存储（含 HttpOnly）。
///
/// ── 为什么必须独立线程 + 超时 ────────────────────────────────
/// Windows 上 `cookies()` 内部要等 WebView2 异步完成回调：在主线程/同步
/// 命令里直接调会与消息循环互相等待而**死锁**（tauri 文档的 Known issues，
/// 现象正是「窗口停住、永不收尾」）。这里挪到 `spawn_blocking` 独立线程，
/// 再包一层 3 秒超时兜底 —— 读不到就当失败，绝不卡死调用方。
async fn read_window_cookies(
    window: &tauri::WebviewWindow,
) -> Result<Vec<tauri::webview::Cookie<'static>>, String> {
    let window_owned = window.clone();
    // 闭包返回类型显式标注：`cookies()` 的返回形态一旦与预期不符，
    // 这里的标注会让编译器直接指出真实类型（而不是在后面 match 处
    // 绕出一串难以定位的推断错误）。
    let handle = tokio::task::spawn_blocking(
        move || -> Result<Vec<tauri::webview::Cookie<'static>>, tauri::Error> {
            window_owned.cookies()
        },
    );
    let nested = tokio::time::timeout(Duration::from_secs(10), handle).await;
    // 三层 Result 逐层展开：Elapsed（超时）→ JoinError（任务失败）→
    // tauri::Error（cookies() 本身），最后拿到 Vec<Cookie>。
    let inner = match nested {
        Ok(result) => result,
        Err(_) => return Err("读取登录窗口 Cookie 超时（WebView2 未响应）".to_string()),
    };
    let joined = match inner {
        Ok(result) => result,
        Err(error) => return Err(format!("读取登录窗口 Cookie 的后台任务失败: {error}")),
    };
    match joined {
        Ok(list) => Ok(list),
        Err(error) => Err(format!("读取登录窗口 Cookie 失败: {error}")),
    }
}

/// 把窗口里的百度域 Cookie 拼成 Cookie 头，返回 `(头串, 是否含 BDUSS)`。
///
/// 交回**全部百度域 Cookie**（不只 BDUSS/STOKEN）：上游 userreport 校验的是
/// 完整浏览器会话，缺 BAIDUID / BAIDUID_BFESS 会被判「未登录」
/// （2026-10-07 浏览器对照实测）。统计类（Hm_* / HMACCOUNT）跳过 —— 对认证
/// 无用，只会把 Cookie 头撑长。
fn kuku_cookie_header(cookies: &[tauri::webview::Cookie<'static>]) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    let mut has_bduss = false;
    let mut seen: Vec<String> = Vec::new();
    for cookie in cookies {
        let name = cookie.name().to_string();
        let domain = cookie.domain().unwrap_or("").to_string();
        if !domain.ends_with("baidu.com") {
            continue;
        }
        if name == "gfprotpl"
            || name.starts_with("Hm_")
            || name == "HMACCOUNT"
            || seen.iter().any(|existing| existing == &name)
        {
            continue;
        }
        if name == "BDUSS" {
            has_bduss = true;
        }
        seen.push(name.clone());
        parts.push(format!("{name}={}", cookie.value()));
    }
    (parts.join("; "), has_bduss)
}

/// 诊断日志：列出 cookie 里所有 BDUSS 候选（domain / path / 长度 / 值前缀）。
///
/// 用于确认「是否有多条 BDUSS（不同域或路径）」「交回时带的是哪一条」——
/// userreport 判「未登录」时，第一步就要排除「带错了候选」。
fn kuku_log_bduss_candidates(cookies: &[tauri::webview::Cookie<'static>]) {
    for cookie in cookies {
        if cookie.name() == "BDUSS" {
            let value = cookie.value();
            let prefix: String = value.chars().take(20).collect();
            eprintln!(
                "[login] KukuAI BDUSS 候选：domain={:?} path={:?} 长度={} 前缀={prefix}…",
                cookie.domain().unwrap_or(""),
                cookie.path().unwrap_or(""),
                value.len()
            );
        }
    }
}

/// KukuAI 登录态交回：用户在主站窗口里登录成功后，读 WebView2 的 Cookie
/// 存储（含 HttpOnly —— 页面脚本 `document.cookie` 读不到 BDUSS），把完整
/// 百度域会话拼成 Cookie 头 POST 网关收尾（见 `read_window_cookies` 的说明）。
async fn submit_kuku_login(
    window: &tauri::WebviewWindow,
    login_state: &str,
) -> Result<(), String> {
    let cookies = read_window_cookies(window).await?;
    let (cookie_header, has_bduss) = kuku_cookie_header(&cookies);
    if !has_bduss {
        return Err("登录窗口没有 BDUSS（登录态尚未写入），请稍后重试".to_string());
    }
    let payload = json!({ "state": login_state, "cookie": cookie_header });
    gateway::call("POST", "/api/session/login/kuku/complete", Some(&payload))
        .await
        .map_err(|error| format!("网关收尾失败: {error}"))?;
    Ok(())
}

/// 用系统默认浏览器打开链接。
///
/// 公开给 commands.rs 复用（打开 Release 页面）：它原来的实现是同一份
/// `cmd /C start`，有下面这个同样的 `&` 截断缺陷 —— 更新说明链接虽然大多
/// 不含查询串，但没有理由留着两份行为不一致的实现。
///
/// ── 为什么不用 `cmd /C start`（曾经的实现，两个真实故障的来源）──
/// 登录 URL 形如 `https://.../login?platform=workbuddy&state=<uuid>`，
/// 而 `cmd` 把 `&` 当**命令分隔符**：实际执行变成
///   ① `start "" https://.../login?platform=workbuddy`
///   ② `state=<uuid>`（一条不存在的命令）
/// 后果有两个，且都很难从现象反推：
///   1. 浏览器打开的登录页**没有 state** —— 用户能正常登录，但上游无法把
///      这次登录与本地发起的任务绑定，后端轮询 `auth/token` 永远返回
///      11217（登录中），前端一直卡在「等待登录完成」直到 5 分钟超时；
///   2. ② 那条命令让 cmd 报错，黑窗口一闪而过。
///
/// 因此改用 `ShellExecuteW`：直接交给 shell 打开 URL，不经过命令解释器，
/// 既不解析 `&`，也不创建控制台窗口。`SW_SHOWNORMAL` 让浏览器正常前台打开。
#[cfg(windows)]
pub fn open_in_browser(url: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    const SW_SHOWNORMAL: i32 = 1;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> *mut std::ffi::c_void;
    }

    let to_wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    };
    let operation = to_wide("open");
    let file = to_wide(url);

    // ShellExecuteW 的返回值 <= 32 表示失败（这是 Win32 的历史约定）
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(format!("打开系统浏览器失败（ShellExecute 返回 {result:?}）"));
    }
    Ok(())
}

/// 非 Windows 平台的等价实现（本项目的打包目标只有 Windows，
/// 保留分支是为了 `cargo check` 在其它平台也能过）
#[cfg(not(windows))]
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // URL 作为独立 argv 传入，不经过 shell，`&` 不会被解释
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开系统浏览器失败: {error}"))
}
