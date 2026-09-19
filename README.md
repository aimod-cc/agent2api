# Agent2API · 多提供商本地网关

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
        └──▶ autoclaw   autoglm-acceleration-api.zhipuai.cn/autoclaw-proxy/proxy/autoclaw
                         X-Authorization: Bearer <token>（OpenAI 兼容）
```

> **本项目仅供学习与交流使用。** 它通过本地反向代理复用你自己账号的登录态，这种「以非官方客户端形态转发」的方式可能不符合上游服务的用户协议，使用风险（含账号被风控、封禁）由使用者自行承担；禁止用于商业用途或绕过计费。详见[使用声明](#使用声明)与 [LICENSE](./LICENSE)。
>
> 本项目是个人用途的本地代理工具，与腾讯（WorkBuddy）、美团（CatPaw）、商汤（小浣熊）、智谱（AutoClaw/autoglm）及其官方产品均无关；所有接口形态来自对各家桌面端通信的观察，上游随时可能调整。

---

## 目录

- [快速开始](#快速开始)
- [网关接口](#网关接口)
- [数据目录](#数据目录)
- [项目结构](#项目结构)
- [开发与构建](#开发与构建)
- [已知限制](#已知限制)
- [使用声明](#使用声明)
- [许可](#许可)

---

## 快速开始

从 Releases 下载安装包（NSIS，简体中文，默认装到 `C:\Program Files\Agent2API`，安装时需要管理员授权），安装后启动即可，**无需安装 Node 或任何其它运行时**：

> 从 1.x 的「按当前用户安装」（`%LOCALAPPDATA%\<产品名>`）升级过来时，新版本启动后会清掉那份旧安装：先确认目录里确实是本产品的主程序才动，然后删除目录、开始菜单/桌面的快捷方式、卸载注册项与失效的开机自启登记；旧目录还在用（删不掉）或不是本产品的会跳过。这段清理只在 release 构建里执行 —— 开发时跑 `tauri dev` 不会影响你本机已装的正式版。数据目录不受影响（迁移是复制）。

1. 首次启动即在应用进程内启动本机网关（端口 3065）并打开主窗口。若检测到 1.x 的旧数据目录 `~/.workbuddy-proxy`，会自动**整体拷贝**为 `~/.agent2api`（旧目录保留不删，可回退），并自动导入三家的旧账号数据（旧网关账号文件与各家桌面端登录态；WorkBuddy 账号随目录拷贝一并带过来）。
2. 点「报表」或「账号」页上的「登录 / 添加账号」，**在弹窗里选提供商**（WorkBuddy / 小浣熊 / CatPaw / AutoClaw），再按该家的方式完成登录或粘贴凭证。WorkBuddy 支持网页登录（内嵌窗口或系统浏览器）与粘贴登录态 JSON；小浣熊支持网页登录、粘贴 token、粘贴 auth.json 与「从本机导入桌面端登录态」；CatPaw 与 AutoClaw 支持填写凭证、粘贴 JSON 与「从本机导入桌面端登录态」。导入桌面端登录态即直接复用桌面客户端自己的登录态文件，账号记录里不落 token，客户端重新登录后网关立刻跟上。
3. 把 OpenAI 客户端的 `base_url` 填成 `http://127.0.0.1:3065/v1`，`api_key` 随便填（例如 `sk-local`，未启用鉴权时服务端不校验）。

关闭窗口默认只是最小化到托盘，网关继续在后台转发；要彻底退出请在托盘图标上右键选「退出」。

四家账号排在同一条**全局队列**里：优先级全局唯一（数值小的先用），界面上的「当前账号」就是队首（把账号置顶即换首选，新账号排到队尾）。转发时按优先级从小到大逐个尝试，跳过已禁用、不提供该模型、以及对该模型处于限额冷却期的账号 —— 于是「先用哪一家」由账号优先级本身决定，没有另一层 provider 路由优先级。某个账号对某个模型触发 429 时会标记「该账号 × 该模型」的冷却并降级到下一个候选，全部候选都不可用才把最后一个真实错误透传出来。

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
- **清单只收录对话模型**（内部补全 / 工具模型、图像与视频模型不会出现在对外目录里），并且**只广告当前有可用登录态的家**。
- 只实现了对话这一条链路：补全、向量、图像、视频等路径没有转发实现（请求得到 404）。
- 上游限流（HTTP 429）时，错误体带 `type: "rate_limit_exceeded"`，并附 `reset_at`（时间戳）与 `reset_at_text`（本地时间文本）。

桌面端界面自用的 `/api/*` 管理接口（账号增删改、模型管理、网关 Key、代理、脱敏、日志、报表、更新）属于内部契约，随界面一起演进，不在这里展开。

---

## 数据目录

所有数据都在 `~/.agent2api/`（Windows 上是 `C:\Users\<你>\.agent2api\`）：`accounts.json`（账号与凭证）、`config.json`（网关配置，含网关 Key 列表、数据保留天数与定时任务设置）、`desensitize.json`（脱敏词表）、`desktop-settings.json`（桌面端偏好）、`logs.jsonl`（系统事件日志）、`requests.jsonl`（请求明细）、`request-daily.jsonl`（按天聚合）、`updates/`（更新下载的安装包）。备份或迁移时整个目录拷走即可；其中 `accounts.json` 含**明文 token**，请勿外传。

事件日志、请求明细与按天聚合各有保留天数（默认 30 / 30 / 365 天，可在设置页「数据」里改），同时事件日志还有一个 500 条的容量上限，两个约束取先到者。

### 定时任务

「定时任务」页集中管理到点自动执行的后台动作，可以逐个开关、调整间隔，后端任务还能立即执行一次（改动立即生效，不必重启程序）：

| 任务 | 默认 | 可配范围 | 说明 |
| --- | --- | --- | --- |
| 自动签到 | 关闭（时刻默认 00:01） | 任意时刻 | 每天签到全部已启用的国内版账号；启动时若当天还没签过会补签一次 |
| 凭证自动维护 | 每 10 分钟 | 1–1440 分钟 | 刷新已过期 / 临期凭证，让账号页显示的有效期保持准确 |
| 模型目录刷新 | 每 60 分钟 | 1–1440 分钟 | 向各提供商拉取最新模型清单；客户端拉 `/v1/models` 时也会顺带刷新一次 |
| 日志页自动刷新 | 每 10 秒 | 5–600 秒 | 停留在「日志」页时的重新拉取间隔 |
| 请求明细自动刷新 | 每 10 秒 | 5–600 秒 | 停留在「请求日志」页时的重新拉取间隔 |

设置写在 `config.json` 的 `scheduledTasks` 字段（自动签到沿用既有的 `autoCheckin` 字段）。两个「自动刷新」管的是页面自身的轮询节奏，页面不可见时都不请求。关掉某个任务只影响它自己的自动执行，不影响在任何页面手动操作。凭证维护与模型刷新用**轮询判定**而不是「睡满间隔」，所以改完间隔最多 10 秒后生效，机器休眠唤醒后也会自动补上错过的时点。

从 1.x 升级时，首次启动会检测 `~/.workbuddy-proxy`：新目录不存在而旧目录存在就把旧目录**整体拷贝**过来（旧目录保留不删），随后自动导入小浣熊 / CatPaw / AutoClaw 三家的旧账号数据（旧网关账号文件 + 各家桌面端登录态）。两者都存在时**以新目录为准**（不合并）。

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
│  │  │  │  │  ├─ mod.rs        ProviderKind（四家）+ PROVIDERS 注册表 + id 互查
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
│  │  │  │  │  └─ autoclaw/     智谱 autoglm：adapter / credentials / refresh / crypto / models / balance
│  │  │  │  ├─ upstream/        转发编排：全局账号队列循环（provider_loop）+ 发送体处理
│  │  │  │  │                    （payload）+ SSE 透传/聚合 + usage 旁路提取
│  │  │  │  ├─ account_store/   账号存储（全局优先级、限额冷却、各家添加与导入）
│  │  │  │  ├─ models/          模型目录底层（workbuddy 内置清单 + /v3/config 刷新）
│  │  │  │  ├─ model_rules.rs   模型管理规则（禁用 / 隐藏 / 映射 alias）
│  │  │  │  ├─ api_keys.rs      网关 Key 列表（多把 Key，任一启用即通过）
│  │  │  │  ├─ auth.rs / auth_http.rs / login.rs   会话、出网传输、无头登录
│  │  │  │  ├─ routing.rs / billing/   账号选路（全局优先级 + 限额冷却）/ 积分签到运营
│  │  │  │  ├─ proxies.rs / clash.rs / egress.rs   出网代理与按出口缓存 Client
│  │  │  │  ├─ desensitize/     脱敏引擎与词表（按 provider + role 生效）
│  │  │  │  ├─ credential_maintenance.rs  已过期 / 临期凭证的批量刷新
│  │  │  │  ├─ scheduled_tasks.rs  间隔型定时任务注册表与调度循环（开关 / 间隔 /
│  │  │  │  │                      上次结果；配置在 config.json 的 scheduledTasks）
│  │  │  │  └─ account_transfer.rs + account_transfer/ / auto_checkin.rs / update/
│  │  │  │                       导入导出（含身份归一）/ 定时签到 / 软件更新
│  │  │  └─ api/                 各路由 handler（health/session/accounts/chat/models/
│  │  │                          keys/model_manage/stats/logs/billing/desensitize/
│  │  │                          auto-checkin/scheduled-tasks/update/…）
│  │  ├─ lib.rs                 应用入口（配置目录迁移 → 设置 → 托盘 → 主窗口 → 启动后端）
│  │  ├─ backend.rs              进程内服务器生命周期
│  │  ├─ legacy_install.rs       旧「当前用户」安装的清理（目录 / 快捷方式 / 卸载项 / 自启；仅 release）
│  │  ├─ gateway.rs              壳侧访问管理 API 的 HTTP 客户端
│  │  ├─ login.rs / commands.rs  登录窗口与轮询、暴露给前端的 invoke 命令
│  │  ├─ bridge.rs               注入 window.workbuddyDesktop 的桥接脚本
│  │  └─ update.rs / settings.rs / state.rs / tray.rs
│  ├─ ui/                        前端（原生 HTML/CSS/JS，无框架）
│  └─ src-tauri/tauri.conf.json  打包配置（NSIS）
├─ docs/agent2api-architecture.md  多提供商改造的架构与契约文档（含各家移植规格）
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

根项目本身没有运行期依赖，`package.json` 只提供上面这些快捷脚本入口。打包产物为 `target/release/bundle/nsis/Agent2API_<版本>_x64-setup.exe`（当前约 2.8 MB；`src-tauri/.cargo/config.toml` 把 cargo 的 `target-dir` 指到了项目根的 `target/`）。

### 关于后端实现

网关是**壳进程内的 Rust HTTP 服务器**（`desktop-tauri/src-tauri/src/server/`），这是项目唯一的后端实现：不随包分发任何外部运行时（旧版曾内置官方 `node.exe`，约 87 MB），没有「升级时杀不掉后端进程」的子进程问题，HTTP 契约与磁盘格式也只有一处定义。

多提供商是「一个 trait + 一张注册表」：`ProviderAdapter` 收拢各家的差异（构造请求 / 错误分类 / 重试建议 / 取凭证 / 刷新清单 / 余额查询 / 登录方式 / 是否会话式等），`adapter_for` 是穷举 match —— 加一家 provider 时编译器会强制给出分支。转发编排只认识 trait 上的能力开关，唯一的一处形态分流是 `is_stateful()`（CatPaw 的会话式转发），具体错误码、鉴权头、退避策略这些 provider 知识全在各家适配器里，不下沉到转发层。

### 前端桥接与测试

界面代码只依赖 `window.workbuddyDesktop` 这一个接口，由 `bridge.rs` 在页面脚本执行前注入（内部把每个方法映射到 Tauri 的 `invoke`），因此 UI 代码里不出现任何 Tauri 字样。前端是原生 HTML/CSS/JS，按页拆成多个模块（`accounts-*.js` / `models-panel.js` / `keys-panel.js` / `requests-panel.js` / `tasks-panel.js` / `usage-*.js` 等），没有打包步骤。仓库当前**没有自动化测试**（Rust 后端没有 `#[test]`，也没有 `tests/` 目录），验证方式是构建后手动跑：

```bash
cargo check --all-targets            # 在 desktop-tauri/src-tauri/ 下，零警告零错误
npm run tauri:dev                    # 开发模式启动，肉眼验证界面与转发
```

手动验证转发链路时，可用 `AGENT2API_PROXY_HOME` 指向一个临时目录，避免影响本机账号数据。

---

## 已知限制

- **只监听 `127.0.0.1`**：Rust 版把监听地址写死在回环地址上，因此无鉴权时也只有本机进程能访问。若同机存在不受信任的程序，建议在「网关 Key」页建一把 Key（一把启用的 Key 都没配时不鉴权；配了之后除 `/health`、`/v1/models`、`/api/session`、`/api/endpoints` 这几个只读探针外的接口都要求带 key）。账号数上限 20 个（全局），token 长度上限 8192 字符，脱敏词表上限 2000 词。
- **端口被占用时不再「复用已运行的服务」**：进程内服务器必须自己绑定成功；若占用者是本产品的旧版进程且位于本应用安装目录内，会自动结束它以完成升级迁移，其余情况给出可操作的错误提示（也可用 `AGENT2API_PROXY_PORT` 换端口）。
- **workbuddy 上游只支持流式**：客户端的 `stream: false` 由网关在内部聚合（拼接 `content` / `reasoning_content`、按 `index` 合并 `tool_calls`、取最后一次出现的 `usage`），代价是拿不到原生非流式行为；首条消息必须是 system，客户端没带时会注入一条兜底系统消息。
- **CatPaw 是「会话式」例外**：上游是多步会话协议（round → turn → 工具循环 → completed），网关要维护会话注册表与指纹链才能接上，分流点是 `is_stateful()`（只有这一家为 true）。它的上游没有可解析的限额信号，因此不会给账号打冷却标记 —— 队列里「一个账号失败就换下一个」的选路仍然生效，但同一账号可能被反复尝试。
- **AutoClaw 的刷新结果不回写桌面端登录态**：那是 safeStorage 加密格式，回写要重新加密且有写坏用户登录态的风险，而且服务端会轮换 refresh_token，网关回写会与桌面端自己的刷新互相顶掉。刷新结果只进进程级缓存（文件 mtime 一变即失效）。手动添加的 AutoClaw 账号仍会回写账号文件。
- **脱敏默认只作用于 `workbuddy`**：其余三家的上游没有这套关键词审核问题，默认勾选只会白白改变报文（可在脱敏页调整作用提供商与角色）。**运营能力（签到 / 自动签到）只有 workbuddy 有**，界面按钮按 provider 隐藏，批量签到只处理 workbuddy 账号 —— 绝不把别家的凭证打去腾讯的签到接口。余额 / 积分查询则是四家都有（各自适配器实现），CatPaw 的余额接口需要单独配置一个网页会话凭证，没配置时面板显示成中性提示而不是失败。
- **优先级强制唯一，且作用域是全局**：这是「主备序号」语义的前提，四家账号共用一条队列，不允许跨家重号。写入侧遇到冲突一律拒绝（409）；启动时若发现历史数据里有并列优先级，会做一次性去重迁移，相对顺序不变。早期版本按 provider 各排各队，那套 provider 路由优先级已随全局队列下线，旧数据会在启动时按原顺序合并成一条队列。
- **代理配置不是「严格镜像」Clash**：账号里只存监听器 uid，端口每次从 `verge.yaml` 实时读取；Clash 里删了监听器，对应账号回退直连并记日志提醒，不会静默换出口。
- **接口形态来自逆向观察**：上游随时可能调整路径、鉴权头或风控策略，届时分别更新 `server/core/endpoints.rs`（workbuddy）与 `server/core/providers/<id>/` 下对应的适配器。事件日志有容量与时间两个保留约束（500 条环形保留 + 可配置保留天数，取先到者），请求明细与按天聚合只有保留天数约束；请求统计不影响转发链路 —— 统计所用的 token 用量是旁路提取的，客户端收到的字节序列与不接统计时完全一致。

---

## 使用声明

### 仅供学习与交流

本项目是一个用于学习 HTTP 反向代理、SSE 流式透传、多上游协议适配与桌面端打包（Tauri）等技术主题的实践项目，**仅供个人学习与研究使用**。它不是官方产品，与腾讯公司及 WorkBuddy / CodeBuddy、美团及 CatPaw、商汤及小浣熊、智谱及 AutoClaw / autoglm 均无任何关联，未获得其授权、认可或赞助。

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

本项目基于 [MIT License](./LICENSE) 开源，可自由使用、修改与分发，须保留版权声明。LICENSE 文件末尾附有上述使用声明，两者共同构成完整的授权与使用约定。
