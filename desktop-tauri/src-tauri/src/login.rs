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
        "catpaw" | "qoder" | "cline-free" | "cline-pass" | "autoclaw" | "autoclaw-intl"
        | "accio" | "accio-cn" | "zcode" | "zcode-intl" => None,
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
/// `social_restore` 勾选时对 workbuddy 额外放行 [`SOCIAL_IDENTITY_HOSTS`]：
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
            matched(allowed)
                || (social_restore && provider == "workbuddy" && matched(SOCIAL_IDENTITY_HOSTS))
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
        "raccoon" => Ok("raccoon"),
        "qoder" => Ok("qoder"),
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
    /// `{done:true, session:…}`
    Done,
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
    PollOutcome::Done
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
        "qoder" => {
            if edition == "intl" || edition == "global" { "intl" } else { "cn" }
        }
        // ZCode 的两个地区由 **provider 本身**决定（界面上是两张卡片，
        // 没有地区下拉，因此面板不会传 `edition`）。不看 `edition` 形参的
        // 后果只是标题里的「国内版/国际版」四个字，但既然信息已经在
        // provider id 里，就不该再去读一个恒为空的形参。
        "zcode" => "cn",
        "zcode-intl" => "intl",
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
        "qoder" => "Qoder",
        "cline-free" => "Cline Free",
        "cline-pass" => "Cline Pass",
        "catpaw" => "CatPaw",
        "raccoon" => "小浣熊",
        // ZCode 两家各自点名，且品牌名里**已经带了地区** —— 因此下面拼标题时
        // 要跳过 edition 后缀，否则会得到「登录 ZCode 国内版 国内版账号」
        "zcode" => "ZCode 国内版",
        "zcode-intl" => "ZCode 国际版",
        _ => "WorkBuddy",
    };
    // 窗口标题：Cline 两家的池、ZCode 两家的地区都已经在品牌名里，
    // 不再拼 edition 后缀（否则会出现「登录 Cline Free 国内版账号」
    // 「登录 ZCode 国内版 国内版账号」这种说不通的标题）
    let title = if matches!(
        provider,
        "cline-free" | "cline-pass" | "zcode" | "zcode-intl"
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
    let social_restore = social_restore && provider == "workbuddy" && edition_id == "intl";
    let result = if use_external {
        open_external(app, &auth_url, edition_label, &login_state).await
    } else {
        run_embedded(app, provider, &auth_url, &title, &login_state, social_restore).await
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
    let result = run_embedded(app, "raccoon", &auth_url, "登录小浣熊账号", &login_state, false).await;
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
        run_embedded(app, "catpaw", &auth_url, "登录 CatPaw 账号", &login_state, false).await
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
            PollOutcome::Done => return Ok(json!({ "ok": true, "external": true })),
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
async fn run_embedded(
    app: &AppHandle,
    provider: &'static str,
    auth_url: &str,
    title: &str,
    login_state: &str,
    social_restore: bool,
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
        .center();

    // 恢复第三方登录入口：登录页按上游开关把 Google / GitHub / X 隐藏了，
    // 注入脚本持续摘掉那段样式（理由与边界见 social_restore_script）。
    //
    // 用 for_all_frames：目标样式在**内层 iframe** 的 head 里，只注入主 frame
    // 够不着它。脚本自身再按 hostname 限一次，避免跳去 accounts.google.com
    // 之后还在那边空转。
    let window = if social_restore {
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
        .on_new_window(move |url, _features| {
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
        match poll_once(login_state).await {
            PollOutcome::Pending => continue,
            PollOutcome::Done => break Ok(json!({ "ok": true })),
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
