# Agent2API · 多提供商本地网关

**简体中文** | [English](./README.en.md)

把多家 AI 桌面客户端的登录态包装成本地 **OpenAI 兼容 API 网关**，统一暴露一个 `base_url`，附带多提供商账号管理、模型管理（启停 / 删除 / 映射）、内容脱敏、出网代理与请求报表，并提供一个开箱即用的 Tauri 桌面端。任何支持自定义 `base_url` 的 OpenAI 客户端都能以 `http://127.0.0.1:3065/v1` 为端点调用这几家的模型额度——不需要 API Key，不需要改客户端源码。

```
OpenAI 客户端 / 任意 SDK
        │  POST /v1/chat/completions   （OpenAI 兼容，SSE）
        ▼
  Agent2API 网关（Rust 进程内服务）              ← 本机 127.0.0.1:3065
  模型映射 · 账号候选链（全局优先级）· 429 降级 · 出网代理 · 内容脱敏
        │  HTTPS（按模型名决定去谁家）
        ├──▶ workbuddy  copilot.tencent.com（国内版）/ www.workbuddy.ai（国际版）
        ├──▶ raccoon    xiaohuanxiong.com/api/web/llm/v2 · Authorization: Bearer <JWT>
        ├──▶ catpaw     ai.catpaw.meituan.com · Cookie: X-Passport-Token=… + user-uid
        │                （自有 conversation 会话协议）
        ├──▶ autoclaw   autoglm-acceleration-api.zhipuai.cn/autoclaw-proxy/proxy/autoclaw
        │                X-Authorization: Bearer <token>（OpenAI 兼容）
        ├──▶ qoder      api3.qoder.sh（国际版）/ gateway.qoder.com.cn（中国版）
        │                COSY 自签名头（不是 Bearer）· 信封式 SSE（自有编码与签名）
        └──▶ cline      api.cline.bot · Authorization: Bearer workos:<JWT>
                         X-CLIENT-TYPE: cline-sdk（缺了它免费池模型一律 403）
                         模型名带池前缀：cline-pass/…（订阅池）· cline-free/…（免费池）
```

> **本项目仅供学习与交流使用。** 它通过本地反向代理复用你自己账号的登录态，这种「以非官方客户端形态转发」的方式可能不符合上游服务的用户协议，使用风险（含账号被风控、封禁）由使用者自行承担；禁止用于商业用途或绕过计费。详见[使用声明](#使用声明)与 [LICENSE](./LICENSE)。
>
> 本项目是个人用途的本地代理工具，与腾讯（WorkBuddy）、美团（CatPaw）、商汤（小浣熊）、智谱（AutoClaw/autoglm）、阿里巴巴（Qoder）、Cline 及其官方产品均无关；所有接口形态来自对各家桌面端通信的观察，上游随时可能调整。

---

## 目录

- [快速开始](#快速开始)
- [数据存储](#数据存储)
- [网关接口](#网关接口)
- [项目结构](#项目结构)
- [开发与构建](#开发与构建)
- [使用声明](#使用声明)
- [许可](#许可)

---

## 快速开始

从 Releases 下载安装包（NSIS，简体中文，默认装到 `C:\Program Files\Agent2API`，安装时需要管理员授权），安装后启动即可，**无需安装 Node 或任何其它运行时**：

> 从 1.x 的「按当前用户安装」（`%LOCALAPPDATA%\<产品名>`）升级过来时，新版本启动后会清掉那份旧安装：先确认目录里确实是本产品的主程序才动，然后删除目录、开始菜单/桌面的快捷方式、卸载注册项与失效的开机自启登记；旧目录还在用（删不掉）或不是本产品的会跳过。这段清理只在 release 构建里执行 —— 开发时跑 `tauri dev` 不会影响你本机已装的正式版。数据目录不受影响（迁移是复制）。

1. 首次启动即在应用进程内启动本机网关（端口 3065）并打开主窗口。若检测到 1.x 的旧数据目录 `~/.workbuddy-proxy`，会自动**整体拷贝**为 `~/.agent2api`（旧目录保留不删，可回退）。账号与历史数据的导入见上一节[数据存储](#数据存储)：若检测到旧版本留下的 JSON / JSONL 数据文件，启动时会弹窗告知并等你点「升级」；那之后三家旧账号数据（旧网关账号文件与各家桌面端登录态）也会一并导入。
2. 点「报表」或「账号」页上的「登录 / 添加账号」，**在弹窗里选提供商**（WorkBuddy / 小浣熊 / CatPaw / AutoClaw / Qoder / Cline），再按该家的方式完成登录或填写凭证。WorkBuddy 只支持网页登录（内嵌窗口或系统浏览器）；小浣熊支持网页登录、填写 token 与「从本机导入桌面端登录态」；CatPaw 支持网页登录、填写凭证与「从本机导入桌面端登录态」；AutoClaw 支持手机验证码登录、填写凭证与「从本机导入桌面端登录态」；Qoder 支持网页登录（国际版与中国版都能登）与个人访问令牌（PAT），网页登录与国际版 / 中国版是同一套设备授权协议，选哪站就登哪站；Cline 支持设备授权登录（打开授权页确认即可，无需粘贴任何东西）、填写 accessToken / refreshToken 与「从本机导入桌面端登录态」。导入桌面端登录态即直接复用桌面客户端自己的登录态文件，账号记录里不落 token，客户端重新登录后网关立刻跟上（Qoder 没有这一项）。
3. 把 OpenAI 客户端的 `base_url` 填成 `http://127.0.0.1:3065/v1`，`api_key` 随便填（例如 `sk-local`，未启用鉴权时服务端不校验）。

关闭窗口默认只是最小化到托盘，网关继续在后台转发；要彻底退出请在托盘图标上右键选「退出」。

五家账号排在同一条**全局队列**里：优先级全局唯一（数值小的先用），「设为首选」把账号移到队首，新账号排到队尾；列表不标记请求正在使用哪个账号。转发时按优先级从小到大逐个尝试，跳过已禁用、不提供该模型、以及对该模型处于限额冷却期的账号 —— 于是「先用哪一家」由账号优先级本身决定，没有另一层 provider 路由优先级。某个账号对某个模型触发 429 时会标记「该账号 × 该模型」的冷却并降级到下一个候选，全部候选都不可用才把最后一个真实错误透传出来。

### 验证

网关起来后，用 curl 确认联通性（`model` 填 `GET /v1/models` 里任一实际存在的名字）：

```bash
curl http://127.0.0.1:3065/health
curl http://127.0.0.1:3065/v1/models

curl http://127.0.0.1:3065/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

客户端接入示例（Python SDK）：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:3065/v1", api_key="sk-local")
resp = client.chat.completions.create(
    model="deepseek-v4.1-flash",     # 名单里有哪家就是哪家，见 GET /v1/models
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

---

## 数据存储

全部数据保存在**一个 SQLite 数据库**里：`~/.agent2api/agent2api.db`（配置目录可用环境变量 `AGENT2API_PROXY_HOME` 覆盖）。库内按用途分表 —— `accounts`（账号）、`logs`（事件日志）、`requests` / `request_daily`（请求明细与按天聚合）、`debug_traffic`（调试模式的原始报文）、`kv`（网关配置与各类零散状态）。设置页「通用 → 数据存储」显示库文件位置、大小与各表条数。

数据库用 WAL 模式，所以运行时同目录下还会有 `agent2api.db-wal` / `agent2api.db-shm` 两个附属文件，备份时请一并带上（或先退出程序，退出时会把 WAL 内容合并回主库）。

> **从旧版本升级**：早期版本把数据分散在 8 个 JSON / JSONL 文件里（`accounts.json`、`config.json`、`logs.jsonl`、`requests.jsonl`、`request-daily.jsonl`、`debug-traffic.jsonl`、`desensitize.json`、`desktop-settings.json`）。新版本首次启动会**检测**到这些文件并弹窗告知「数据结构已换成 SQLite」，你点弹窗里的「升级」按钮后才开始导入；点「稍后」则本次不导入（账号与历史记录暂时不可用，下次启动照常提示）。
>
> 导入成功后，旧文件**只改名**为 `原名.migrated`（例如 `accounts.json.migrated`）作为备份留在原处，**不会被删除**。要回退或人工核对数据，随时可以打开这些文件；把某个文件改回原名再重启，程序会重新提示升级。

---

## 网关接口

对外只有 OpenAI 兼容的对话链路（默认 `http://127.0.0.1:3065`）：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/v1/chat/completions` | 对话。`stream: true` 走 SSE 流式透传；`stream: false` 由网关内部聚合后返回完整 JSON |
| GET | `/v1/models` | 聚合模型列表（OpenAI `list` 格式，附上下文长度与能力标记，`owned_by` 是实际承载的 provider id） |
| GET | `/health` | 健康检查（是否已配置登录态、上游地址、token 过期时间） |

- **模型名默认用上游原始名**，网关不加前缀。清单是各家的聚合结果，同名模型的条目保留注册表顺序里靠前的那家（`owned_by` 记为实际承载它的 provider），但请求走哪家由**账号优先级**决定 —— 候选链是「提供该模型的各家账号按全局优先级排队」，逐个尝试，全部失败才把最后一个真实错误透传出来。
- 点名的模型必须真实存在于目录中，否则直接返回 400（`code: "model_not_found"`）并附近似名提示 —— 把 `deepseek-v4.1-flash` 悄悄换成别的模型会造成「请求 A 实跑 B」这类难以察觉的事故，所以不做静默回退。
- **模型管理页可以起映射名**：为某个上游模型配一个对外的别名（alias → target），下游用别名请求时网关改写成目标模型再转发；别名也会作为独立条目出现在 `/v1/models` 里（`is_default` 恒为 false）。同一别名只能指向一个目标，且不得与任何上游模型 id 重名。禁用或删除的模型不出现在 `/v1/models`，请求它返回 400 `model_not_found`（删除只从清单隐藏，可在管理页「已删除」筛选里恢复）。
- **带前缀的上游模型名会自动配一条去前缀映射**（Cline 是当前唯一这样的家）：它的模型名带**计费通道前缀** —— `cline-pass/…` 是 ClinePass 订阅池、`cline-free/…` 是免费额度池，前缀必须原样发给上游（剥掉会 404）。网关为每个带前缀的模型自动加一条映射（`glm-5.3` → `cline-pass/glm-5.3`），因此**两种名字都能用**：写 `glm-5.3` 或写完整 id 都行。这条映射和手工加的映射一样在模型管理页可见、可删；删掉后网关不会再自动加回来。两个池有同名模型时（如 `deepseek-v4.1-flash`）**先到先得**：先处理的那个池拿到短名，另一个池只用完整 id 点名（`/v1/models` 里两个完整 id 都在）。
- **清单只收录对话模型**（内部补全 / 工具模型、图像与视频模型不会出现在对外目录里），并且**只广告当前有可用登录态的家**。
- 只实现了对话这一条链路：补全、向量、图像、视频等路径没有转发实现（请求得到 404）。
- 上游限流（HTTP 429）时，错误体带 `type: "rate_limit_exceeded"`，并附 `reset_at`（时间戳）与 `reset_at_text`（本地时间文本）。

桌面端界面自用的 `/api/*` 管理接口（账号增删改、模型管理、网关 Key、代理、脱敏、日志、报表、更新）属于内部契约，随界面一起演进，不在这里展开。

---

## 项目结构

网关与桌面端都在 `desktop-tauri/`：后端是 `src-tauri/` 下的 Rust 进程内 HTTP 服务器，前端是 `ui/` 下的原生 HTML/CSS/JS。

```
agent2api/
├─ desktop-tauri/
│  ├─ src-tauri/src/
│  │  ├─ server/                 网关实现（Rust，进程内 HTTP 服务器）
│  │  │  ├─ mod.rs               服务组装：ServerState、启动、停机、启动迁移
│  │  │  ├─ http.rs              路由表、CORS、API Key 中间件、body 限制
│  │  │  ├─ config.rs / logging.rs / logs_store.rs / errors.rs
│  │  │  ├─ config_migration.rs  1.x 配置目录迁移（~/.workbuddy-proxy → ~/.agent2api，启动第一步）
│  │  │  ├─ request_stats.rs + request_stats/   统计的时钟窗口、写入、聚合与裁剪
│  │  │  ├─ core/
│  │  │  │  ├─ providers/        ★ 多提供商层（本改造的核心）
│  │  │  │  │  ├─ mod.rs        ProviderKind（五家）+ PROVIDERS 注册表 + id 互查
│  │  │  │  │  ├─ adapter.rs    ProviderAdapter trait + adapter_for + implemented_kinds
│  │  │  │  │  ├─ router.rs     模型名 → 候选 provider 集合（聚合目录）
│  │  │  │  │  ├─ catalog.rs    聚合模型目录（清单合并 / 同名去重 / 可用性判定）
│  │  │  │  │  ├─ refresh_flight.rs  凭证刷新的单飞去重
│  │  │  │  │  ├─ workbuddy.rs  WorkBuddy 适配器（头集合 / system 注入 / 6004 / 11128）
│  │  │  │  │  ├─ raccoon/      小浣熊：mod / models / credentials / jwt / oauth / balance
│  │  │  │  │  ├─ catpaw/       CatPaw：adapter（is_stateful）/ conversation（轮次状态机）/
│  │  │  │  │  │                turn_executor / prepare / decision（轮次判定）/ fingerprint /
│  │  │  │  │  │                registry/（会话注册表：表与句柄 / 账号身份 / 作废）/
│  │  │  │  │  │                messages / blocks / tools / openai（翻译层）/
│  │  │  │  │  │                upstream_http / image_compress / models / credentials / balance
│  │  │  │  │  ├─ autoclaw/     智谱 autoglm：adapter / credentials / refresh / crypto / models /
│  │  │  │  │  │                balance / login（手机验证码）/ checkin（每日签到任务）
│  │  │  │  │  ├─ qoder/        Qoder：adapter / endpoints（两站地址）/ oauth（设备授权）/
│  │  │  │  │  │                auth / cosy（COSY 签名与体编码）/ protocol（信封解码）/
│  │  │  │  │  │                chat（会话式转发）/ stream / machine（PKCE 与机器标识）/
│  │  │  │  │  │                credentials / refresh / models / balance
│  │  │  │  │  └─ cline/        Cline：adapter（Bearer + 产品面头）/ credentials（workos: 前缀
│  │  │  │  │                   + 桌面端登录态 + 姓名解析）/ login（WorkOS 设备授权）/
│  │  │  │  │                   refresh（单飞续期）/ models（两额度池 + 默认映射种子）/
│  │  │  │  │                   balance（credit 余额，微 credit ÷1e6）
│  │  │  │  ├─ upstream/        转发编排：全局账号队列循环（provider_loop）+ 发送体处理
│  │  │  │  │                    （payload）+ SSE 透传/聚合 + usage 旁路提取
│  │  │  │  ├─ account_store/   账号存储（全局优先级、限额冷却、各家添加与导入）
│  │  │  │  ├─ models/          模型目录底层（workbuddy 内置清单 + /v3/config 刷新）
│  │  │  │  ├─ model_rules.rs   模型管理规则（禁用 / 隐藏 / 映射 alias）
│  │  │  │  ├─ api_keys.rs      网关 Key 列表（多把 Key，任一启用即通过）
│  │  │  │  ├─ auth.rs / auth_http.rs / login.rs   会话、出网传输、无头登录
│  │  │  │  │                    （login/ 下是各家特有的登录流程：catpaw 的
│  │  │  │  │                    loopback 回调、qoder 的设备授权）
│  │  │  │  ├─ routing.rs / billing/   账号选路（全局优先级 + 限额冷却）/ 积分签到运营
│  │  │  │  ├─ proxies.rs / clash.rs / egress.rs   出网代理与按出口缓存 Client
│  │  │  │  ├─ desensitize/     脱敏引擎与词表（按 provider + role 生效）
│  │  │  │  ├─ credential_maintenance.rs  已过期 / 临期凭证的批量刷新
│  │  │  │  ├─ usage_query.rs     余额 / 积分查询（跨账号并发 + 定时那一轮的快照）
│  │  │  │  ├─ scheduled_tasks.rs  间隔型定时任务注册表与调度循环（开关 / 间隔 /
│  │  │  │  │                      上次结果；配置在 config.json 的 scheduledTasks）
│  │  │  │  └─ account_transfer.rs + account_transfer/ / auto_checkin.rs / update/
│  │  │  │                       导入导出（含身份归一）/ 定时签到 / 软件更新
│  │  │  └─ api/                 各路由 handler（health/session/accounts/accounts_usage/
│  │  │                          chat/models/keys/model_manage/stats/logs/billing/
│  │  │                          desensitize/auto-checkin/scheduled-tasks/update/…）
│  │  ├─ lib.rs                 应用入口（配置目录迁移 → 设置 → 托盘 → 主窗口 → 启动后端）
│  │  ├─ backend.rs              进程内服务器生命周期
│  │  ├─ legacy_install.rs       旧「当前用户」安装的清理（目录 / 快捷方式 / 卸载项 / 自启；仅 release）
│  │  ├─ gateway.rs              壳侧访问管理 API 的 HTTP 客户端
│  │  ├─ login.rs / commands.rs  登录窗口与轮询、暴露给前端的 invoke 命令
│  │  ├─ login_profile.rs        每次网页登录独享一个临时 WebView2 数据目录（登完即删）
│  │  ├─ bridge.rs               注入 window.workbuddyDesktop 的桥接脚本
│  │  └─ update.rs / settings.rs / state.rs / tray.rs
│  ├─ ui/                        前端（原生 HTML/CSS/JS，无框架）
│  └─ src-tauri/tauri.conf.json  打包配置（NSIS）
├─ build/make-icon.mjs           生成应用图标源图
└─ package.json                  构建脚本入口（tauri:dev / tauri:build / build:icon）
```

---

## 开发与构建

### 环境要求

- Rust >= 1.77 与 Tauri 2 工具链（编译桌面端本体；Windows 上还需 WebView2 运行时）
- Node.js >= 18.17（仅用来执行 `npm run tauri:*` 与 `build/make-icon.mjs` 这些前端构建脚本，桌面端运行时不依赖 Node，也不会打包任何 Node 产物）

### 常用脚本

```bash
npm run tauri:install      # 安装桌面端依赖（等价 npm --prefix desktop-tauri install）
npm run tauri:dev          # 开发模式调起桌面端（自动热重载）
npm run tauri:build        # 构建桌面端安装包

npm run build:icon         # 生成图标源图（改图标设计后执行，再跑 tauri icon）
```

根项目本身没有运行期依赖，`package.json` 只提供上面这些快捷脚本入口。打包产物为 `target/release/bundle/nsis/Agent2API_<版本>_x64-setup.exe`（当前约 3.0 MB；`src-tauri/.cargo/config.toml` 把 cargo 的 `target-dir` 指到了项目根的 `target/`）。

---

## 使用声明

### 仅供学习与交流

本项目是一个用于学习 HTTP 反向代理、SSE 流式透传、多上游协议适配与桌面端打包（Tauri）等技术主题的实践项目，**仅供个人学习与研究使用**。它不是官方产品，与腾讯公司及 WorkBuddy / CodeBuddy、美团及 CatPaw、商汤及小浣熊、智谱及 AutoClaw / autoglm、阿里巴巴及 Qoder 均无任何关联，未获得其授权、认可或赞助。

### 关于反向代理行为

本项目实现的是本地反向代理：在你自己的机器上复用你自己账号的登录态，把请求转发到官方上游网关。它不破解、不绕过任何付费或权限校验，使用的额度始终来自你自己账号本就拥有的配额。但需要明确的是，这种「以非官方客户端形态复用登录态」的转发方式，**可能不符合上游服务的用户协议或使用条款**；是否使用、由此产生的一切后果（包括但不限于账号被限流、被风控、被冻结或封禁），均由使用者自行承担。

### 禁止用途

禁止将本项目用于任何商业用途、二次分发牟利、批量账号运营、绕过上游计费或配额限制、或其他违反当地法律法规的活动。如需在生产环境或商业场景中调用相关模型服务，请使用官方渠道与官方 API。

### 凭证与数据风险

本项目会把账号凭证（`accessToken` / `refreshToken` 等）以**明文**形式保存在本机配置目录（默认 `~/.agent2api/`）中，导出功能生成的文件同样包含明文凭证。请自行妥善保管，切勿提交到公开仓库、上传到网盘或分享给他人。因凭证泄露造成的损失由使用者自行承担。

### 无担保与权利通知

本项目按「现状」提供，作者不对其可用性、稳定性、安全性或对特定用途的适用性作任何承诺。上游接口随时可能变更，本项目可能随时失效而不再维护。完整条款见 [LICENSE](./LICENSE)。本项目中的接口形态、协议字段等信息来自对各官方客户端公开网络通信的观察与整理，相关商标与服务的权利归其各自所有者；若权利方认为本项目存在不当之处，请联系作者，将及时调整或删除。

---

## 许可

本项目基于 [MIT License](./LICENSE)，可自由使用、修改与分发，须保留版权声明。

需要留意的是：LICENSE 正文之后附有一份**使用声明**，其中第 3 条在 MIT 之上**追加了限制**（禁止商业用途、禁止二次分发牟利、禁止批量账号运营）。因此本项目**不是**纯粹的 MIT 项目——**MIT 条款与使用声明共同构成完整的授权与使用约定**，两者对同一行为给出不同结论时以更严格的一方为准。这也是 `Cargo.toml` 用 `license-file` 指向 LICENSE、而不声明 SPDX `"MIT"` 的原因。
