# WorkBuddy Local Proxy

把腾讯 WorkBuddy 桌面端的登录态包装成本地 **OpenAI 兼容 API 网关**，附带多账号管理、积分查询、每日签到、出网代理、内容脱敏与运行日志，并提供一个开箱即用的 Tauri 桌面端。

装上桌面端后，任何支持自定义 `base_url` 的 OpenAI 客户端都能以 `http://127.0.0.1:3065/v1` 为端点直接使用 WorkBuddy 的模型额度——不需要 API Key，不需要改客户端源码。

```
OpenAI 客户端 / 任意 SDK
        │  POST /v1/chat/completions   （OpenAI 兼容，SSE）
        ▼
  本网关（Rust 进程内服务）                        ← 本机 127.0.0.1:3065
  登录态复用 · 多账号选路 · 429 降级 · 出网代理 · 内容脱敏
        │  HTTPS
        ▼
  https://copilot.tencent.com/v2/chat/completions   （国内版）
  https://www.workbuddy.ai/v2/chat/completions      （国际版）
```

> **本项目仅供学习与交流使用。** 它通过本地反向代理复用你自己账号的登录态，这种「以非官方客户端形态转发」的方式可能不符合上游服务的用户协议，使用风险（含账号被风控、封禁）由使用者自行承担；禁止用于商业用途或绕过计费。详见[使用声明](#使用声明)与 [LICENSE](./LICENSE)。
>
> 本项目是个人用途的本地代理工具，与腾讯官方无关。所有接口形态来自对 WorkBuddy 桌面端通信的观察，上游随时可能调整。

---

## 目录

- [主要特性](#主要特性)
- [快速开始](#快速开始)
- [HTTP 接口](#http-接口)
- [核心概念](#核心概念)
- [配置项](#配置项)
- [数据目录](#数据目录)
- [项目结构](#项目结构)
- [开发与构建](#开发与构建)
- [设计取舍与已知限制](#设计取舍与已知限制)
- [使用声明](#使用声明)
- [许可](#许可)

---

## 主要特性

**OpenAI 兼容转发**

- 对话链路：`POST /v1/chat/completions` 与 `GET /v1/models`。多账号选路、429 降级、请求去重、内容脱敏、SSE 透传/聚合都实现在这条链路上。
- 上游只支持流式（`stream:false` 会被上游拒绝），网关对声明 `stream:false` 的客户端在内部以流式请求、把 SSE 聚合成完整 JSON 返回，客户端无需关心。
- 复刻桌面端的请求头（`Authorization` / `X-User-Id` / `X-Enterprise-Id` / `X-IDE-*` / `X-Product` / 链路追踪头），并按账号版本区分 UA（国内版 `WorkBuddy`、国际版 `WorkBuddy AI`）。UA 里的 `CLI/<版本>` 段必须带，否则上游只下发精简版模型清单。
- 思考内容（`reasoning_content`）小分片会合并到 60 字符以上再下发，避免客户端把思考渲染成一堆碎块。

**多账号**

- 无头登录（浏览器完成一次授权）或手动粘贴 `accessToken` / `refreshToken`，最多保存 20 个账号，国内版与国际版账号可混存、按账号各自路由。
- 严格优先级队列（0–9999，全局唯一），当前使用哪个账号由排序派生，不存在「手动选 A 实际用 B」的分叉。
- 账号对某个模型触发限额（HTTP 429 / 上游 `code 6004`）时，自动标记该账号×该模型的冷却期并降级到下一个账号重试，恢复时间从上游提示文本解析；请求成功即清除标记。
- 支持批量启用 / 禁用 / 改代理 / 删除，支持账号导入导出（导出文件含凭证，换机器导入即可沿用登录态）。

**运营能力**

- 积分/额度查询（个人账号走资源包接口，企业账号走企业额度接口，不限量用哨兵值表示）。
- 每日签到与一键签到全部账号（串行执行，避免触发上游风控；国际版无签到活动，会被明确跳过或报错）。
- **自动签到**：在设置页指定每天的执行时刻（默认 `00:01`），到点自动签到全部已启用的国内版账号。若启动时当天时间点已过且尚未签到，会立即补签一次，不会因为当时没开机而漏掉；同一天只执行一次，上游接口幂等，重复调用只返回「已领取」。
- 运营 banner 与大使状态查询。

**软件更新**

- 设置页可直接检查 GitHub Release 是否有新版本，展示版本号与发布说明，一键下载安装包并启动安装程序（自动退出本程序以让出文件占用，安装完成后重启）。
- 检测与下载由本地网关发起（桌面壳的 HTTP 客户端不启用 TLS，发不出 GitHub 请求）。直连失败时自动尝试经 Clash Verge 混合端口重试一次。
- 下载地址限定在 GitHub 域名内，安装包运行前会校验「存在、`.exe`、且位于受控下载目录」三项，避免该功能被当成任意程序执行入口。

**网络与安全**

- 按账号指定出网代理：可选 Clash Verge 的节点监听器或全局混合端口（端口实时读取，不在本工具里复制一份），也可填自定义 HTTP / SOCKS5 代理。代理解析失败自动回退直连并记日志。
- 出口连通性测试（以「能否连上 WorkBuddy 上游」为判据，而不是能否访问墙外站点），附带出口 IP 查询。
- 敏感词脱敏：命中词内部插入零宽空格（U+200B），人眼与模型读到的不变，但上游后端的关键词匹配会失效，用于化解合规模板被内容审核误拦的问题。默认开启，词表与作用角色可在桌面端维护。

**可观测性**

- **报表页**：时间范围（今天 / 7 天 / 30 天 / 本月 / 全部，选择会记住）内的总请求数、成功率、总 Token、活跃天数与连续使用天数、Top 模型占比；近 365 天活跃热力图（按当天请求量分档着色）；缓存命中率的四个时间窗口（10 分钟 / 1 小时 / 24 小时 / 7 天）与近 24 小时命中率折线、按天 Token 柱状图。
- **日志页双视图**：「系统事件」记录登录、账号切换、429 降级这类事件；「模型请求」逐条记录转发到上游的每次请求（模型、承载账号、HTTP 状态、耗时、token 用量与缓存读取量、失败原因），支持时间范围与成功/失败筛选、服务端分页。两个视图的筛选条件各自独立保存。
- 运行日志落到 `logs.jsonl`，分 `debug/info/warn/error` 四级与 7 个分类，429 自动切换带结构化的「原账号 → 目标账号 + 恢复时间」字段。
- **数据保留天数可配置**：事件日志、请求明细、按天聚合三档各自独立（1–3650 天，默认 30 / 30 / 365），改完立即生效、无需重启；改小会删除超出的历史数据，保存前有二次确认。
- 设 `WORKBUDDY_VERBOSE=1` 后输出每个入站请求与上游交互的细节。另外，最近一次入站请求体始终会落盘到 `debug/last-request.json`（覆盖写，便于重放分析），与该开关无关；同目录的 `last-request.meta.txt` 记一行时间戳与请求摘要。

**桌面端**

- Tauri 2 打包，**后端网关以 Rust 重写并运行在应用进程内**，随包不携带任何外部运行时（`node.exe` 已移除），安装包约 **2.4 MB**，安装后开箱即用。
- 六个页面：报表、账号、网关、脱敏、日志、设置。设置页为左侧分类导航（通用 / 账号 / 服务 / 数据）加右侧内容栏；支持明暗主题与跟随系统、系统托盘、开机自启、关闭窗口最小化到托盘、单实例。
- 交互细节：下拉选择为浮层列表（支持键盘操作与选中标记），设置项的长说明收在标题旁的问号气泡里，账号卡片可直接启用 / 禁用。
- 覆盖升级时会自动收口旧版本残留：若检测到旧版网关进程仍占用 3065 端口（例如旧版被强杀后留下的孤儿 `node` 进程），确认是本产品进程且位于本应用安装目录内才会结束它，并清理安装目录里遗留的 `resources\node.exe` 与 `resources\server.cjs`。用户自己在安装目录外另起的服务，绝不会被误杀。

---

## 快速开始

### 安装与启动

从 Releases 下载安装包（NSIS，简体中文，按当前用户安装），安装后启动即可，**无需安装 Node 或任何其它运行时**：

1. 首次启动即在应用进程内启动本机网关（端口 3065）并打开主窗口。
2. 点「报表」或「账号」页上的「登录 / 添加账号」，选账号版本（国内版 / 国际版）与登录方式（内嵌窗口 / 系统默认浏览器），完成一次官方登录。
3. 把 OpenAI 客户端的 `base_url` 填成 `http://127.0.0.1:3065/v1`，`api_key` 随便填（例如 `sk-local`，未启用鉴权时服务端不校验）。

关闭窗口默认只是最小化到托盘，网关继续在后台转发；要彻底退出请在托盘图标上右键选「退出」。

从 1.0.x 覆盖升级时无需手工处理旧进程：新版启动会自动结束旧版遗留的网关进程并清理安装目录里的旧运行时文件（`node.exe` / `server.cjs`，判定条件见[桌面端](#主要特性)一节）。

### 验证

网关起来后，用 curl 确认联通性：

```bash
curl http://127.0.0.1:3065/health
curl http://127.0.0.1:3065/v1/models

curl http://127.0.0.1:3065/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

客户端接入示例（Python SDK）：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:3065/v1", api_key="sk-local")
resp = client.chat.completions.create(
    model="deepseek-v4.1-flash",
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

### 模型选择

`GET /v1/models` 返回的清单有两个来源：内置兜底清单，以及运行时从上游 `GET /v3/config` 拉回的真实清单（后者在启动和每次访问 `/v1/models` 时异步刷新）。默认模型是 `auto`。

清单**只收录对话模型**：判定规则见 `desktop-tauri/src-tauri/src/server/core/models.rs` 的 `is_chat_model`，非对话项（内部补全/工具模型、图像/视频模型）不会出现在对外目录里。

**网关不会静默改写模型名**：请求里点名的模型必须在目录中真实存在，否则直接返回 400 并附上近似名提示（`model_not_found`）。这是有意为之——把 `deepseek-v4.1-flash` 悄悄换成别的模型会造成「请求 4.1 实跑 V4」这类难以察觉的事故。需要收窄可选模型时，在 `desktop-tauri/src-tauri/src/server/core/models.rs` 的 `MODEL_ALLOWLIST` 填入白名单（默认空数组即全量放行）。

---

## HTTP 接口

所有管理接口的响应信封为 `{ success: true, data }` 或 `{ success: false, error }`；OpenAI 兼容接口则严格遵循 OpenAI 的错误结构（`{ error: { message, type, ... } }`）。

### OpenAI 兼容

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/v1/chat/completions` | 对话，SSE 流式透传；`stream:false` 由网关内部聚合 |
| GET | `/v1/models` | 模型列表（OpenAI `list` 格式，附带 `credits`、上下文长度、能力标记） |

> 只暴露对话这一个模型端点。逆向出的接口清单里还有 `/v2/completions`、`/v2/embeddings`、`/v2/images/generations`、`/v2/videos/generations` 等路径，但都未经实测（上游是否开放、body 结构是否一致均未知），因此没有实现转发，请求它们会得到 404。

限额错误的返回体带 `type: "rate_limit_exceeded"`，并附加 `reset_at`（时间戳）与 `reset_at_text`（本地时间文本）。

### 会话与配置

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/health` | 健康检查（是否已配置登录态、上游地址、账号 ID、token 过期时间） |
| GET | `/api/session` | 会话总览：健康状态、账号列表、模型目录、脱敏状态、首选账号、最近请求模型 |
| POST | `/api/session/login/start` | 发起登录，返回 `{ state, authUrl }` |
| GET | `/api/session/login/wait?state=` | 轮询登录结果（`pending: true` 表示仍在等待） |
| POST | `/api/session/login/cancel` | 取消登录（中止后端轮询） |
| POST | `/api/session/refresh` | 刷新首选账号 token |
| POST | `/api/session/logout` | 清除登录态（删除首选账号） |
| GET | `/api/config` | 读取网关配置（API Key 掩码、计费语言、默认模型、脱敏状态） |
| POST | `/api/config` | 修改 API Key 与计费语言 |
| GET | `/api/endpoints` | 打印全部已逆向的上游接口清单（排查用） |
| GET | `/api/activity/banner` | 运营 banner |
| GET | `/api/activity/ambassador` | 大使状态 |
| POST | `/auth/login` \| `/auth/logout` | 旧版登录入口（保留兼容） |

### 账号管理

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/accounts` | 账号列表（含首选账号、优先级、启用状态、代理、限额记录） |
| POST | `/api/accounts` | 手动添加账号（粘贴 credential JSON，可带备注名与版本） |
| PATCH | `/api/accounts/<id>` | 修改备注名 / 优先级 / 启用状态 / 代理 |
| POST | `/api/accounts/<id>/move` | 与相邻账号交换优先级（`direction: up \| down`） |
| POST | `/api/accounts/current` | 把账号置顶，即设为当前（首选）账号 |
| POST | `/api/accounts/batch` | 批量操作（`action: enable \| disable \| proxy \| remove`） |
| DELETE | `/api/accounts/<id>` | 删除账号 |
| POST | `/api/accounts/refresh` | 刷新指定（缺省首选）账号 token |
| GET | `/api/accounts/usage` | 批量查询各账号积分/额度（并发，单个失败不影响其他） |
| POST | `/api/accounts/checkin` | 批量/单个签到 |
| GET | `/api/accounts/export` | 导出全部账号（含凭证） |
| POST | `/api/accounts/import` | 导入账号（merge 语义，按 uid 匹配） |

### 代理、脱敏、日志、积分

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/proxies` | 出网代理可选项（Clash Verge 实时快照 + 各账号当前出口） |
| POST | `/api/proxies/test` | 出口连通性测试（`{ id }` 测某账号，`{ proxy }` 测临时配置，`proxy: null` 测直连） |
| GET | `/api/usage` | 积分/额度查询 |
| GET | `/api/checkin/status` | 签到活动状态 |
| POST | `/api/checkin` | 领取每日签到积分 |
| POST | `/api/checkin/claim-and-report` | 签到并回报最新积分 |
| GET | `/api/auto-checkin` | 自动签到状态（开关、触发时刻、下次执行、上次结果） |
| POST | `/api/auto-checkin` | 修改自动签到设置（`{ enabled?, time? }`，`time` 为 `HH:MM`） |
| POST | `/api/auto-checkin/run` | 立即执行一次全部账号签到 |
| GET | `/api/update/check?current=1.0.0` | 检查 GitHub Release 是否有新版本 |
| POST | `/api/update/download` | 下载安装包（`{ url, name }`，仅接受 GitHub 域名） |
| GET | `/api/update/progress` | 下载进度 |
| POST | `/api/update/cancel` | 取消下载（清理半截文件） |
| GET | `/api/desensitize` | 脱敏状态（开关、词表、角色、命中统计） |
| POST | `/api/desensitize/enabled` | 开关脱敏 |
| POST | `/api/desensitize/roles` | 设置作用角色 |
| PUT / POST / DELETE | `/api/desensitize/terms` | 全量替换 / 追加 / 删除词 |
| POST | `/api/desensitize/reset` | 恢复默认词表 |
| POST | `/api/desensitize/stats/reset` | 清空命中统计 |
| GET | `/api/logs` | 查询运行日志（`limit` / `level` / `category` / `keyword` / `sinceId` / `start` / `end`，后两个为毫秒时间戳） |
| GET | `/api/logs/stats` | 各级别与分类计数 |
| GET | `/api/logs/download` | 导出 JSONL 附件 |
| DELETE | `/api/logs` | 清空日志 |
| GET | `/api/stats/summary` | 报表聚合（`range=today \| 7 \| 30 \| month \| all`，缺省 `7`）：概览、热力图、缓存命中率、趋势 |
| GET | `/api/stats/requests` | 请求明细（`offset` / `limit` / `model` / `status` / `start` / `end`，服务端分页） |
| DELETE | `/api/stats/requests` | 清空请求明细与按天聚合 |
| GET | `/api/retention` | 读取三档数据保留天数 |
| PUT | `/api/retention` | 更新保留天数并立即触发清理（`{logRetentionDays?, requestRetentionDays?, dailyRetentionDays?}`） |

设置了 API Key 后，除 `/health`、`/v1/models`、`/api/session`、`/api/endpoints` 这几个只读探针接口外，其余接口都需要在 `Authorization: Bearer <key>` 或 `X-API-Key` 头里带上它。

---

## 核心概念

### 转发顺序与「首选账号」

账号的 `priority` 是**严格的主备序号**：数值小的先用，且**必须全局唯一**。允许并列会让「谁先用」失去意义（同级退化成永远只取加入最早的那个，其余账号形同虚设），因此写入侧遇到冲突一律拒绝（409），由用户显式选一个空闲值。启动时若发现历史数据里有并列优先级，会做一次性去重迁移，并保持原有相对顺序不变。

界面上的「首选账号 / 当前账号」不是一个独立保存的手动选择，而是**由优先级派生**的结果——转发顺序里第一个「已启用且有凭证」的账号。因此界面标注的「首选」与网关默认使用的账号始终一致，不存在两套状态打架的可能。要换首选账号，就把目标账号置顶（`POST /api/accounts/current`），其余账号会整体让位并连续重新编号。新添加的账号排在队尾，不会抢占正在使用的账号。

选路时会跳过两类账号：`enabled: false` 的禁用账号，以及对该次请求所用**模型**处于限额冷却期的账号。若启用中的账号都在该模型限额期内，网关会挑恢复最早的那个再试一次——上游可能已经实际解除限额，成功则直接返回，否则把上游真实的 429 与恢复时间透传给客户端。若全部账号都被禁用，则明确报 503，绝不回退到已禁用账号。

### 按模型的限额降级

限额记录是**账号 × 模型**维度的（上游 429 / `code 6004` 的提示文本里带 UTC+8 的重置时间）。这意味着同一个账号对 `deepseek-v4.1-flash` 限额时，换成 `glm-5.0` 可能仍然可用——「首选账号」是模型无关的，只有具体某次请求会按模型临时降级到下一个候选。想看清「我正在用的这个模型会走哪些账号」，用账号页顶部的「模型」筛选（默认选中最近一次实际请求所用的模型），列表显示的限额与首选判定都按该模型计算。

### 出网代理

默认所有请求直连上游。需要经代理访问时，可以给每个账号单独指定出口：

- **Clash Verge 监听器**：只记录监听器 uid，端口每次从 `verge.yaml` 实时读取（3 秒 TTL 缓存）。在 Clash 里改端口不必回来重配；删掉某个监听器后，引用它的账号会解析失败并回退直连，同时在日志里提醒。
- **自定义代理**：`{ protocol: 'http' | 'socks5', host, port, username?, password? }`。

出网统一走 `desktop-tauri/src-tauri/src/server/core/egress.rs`：按出口缓存 `reqwest::Client`（`Client` 自带连接池，因此「一个出口一个 Client」就是「一个出口一个连接池」），否则每个请求都要重新和代理建一条 TCP 连接。缓存最多保留 24 个出口，超出按「最久未用」淘汰。为了让「能转发就一定能刷 token」，token 续期与计费查询也复用账号自己的出口。

### 内容脱敏

客户端自带的 system 合规模板里有一批「拒绝协助 DoS 攻击 / 漏洞利用开发 / 凭证测试」之类的高频词，这些词本身是安全声明，却容易触发上游后端的关键词审核导致请求被误拦。网关的做法是在命中词内部插入零宽空格：

```
DoS  →  D​oS      （中间是一个 U+200B，人眼与模型读到的不变）
```

默认开启，默认作用于 `system` 与 `user` 两种角色（可扩展到 `assistant` / `tool` / `developer`），内置 32 个默认词，词表上限 2000 词、单词 200 字符。纯 ASCII 词会加词边界，避免 `0day` 命中 `100days`、`XSS` 命中 `XSSRF` 这类误伤。词表持久化在 `~/.workbuddy-proxy/desensitize.json`，命中统计在内存中累计。

### 防重试风暴

客户端请求失败后常对同一请求体快速重试，高频重复请求会触发上游频率风控（`code 11128`）并互相续期。网关做了两层防护：

- **请求合并**：相同 body（sha256 去重键）的请求在代理内排队串行，等待上限 45 秒。
- **风控退避**：命中 `11128` 时按 10 秒 / 25 秒退避重试，最多 2 次。间隔故意拉长，因为拉黑期间的每次重试都会给黑名单续期，宁可让客户端拿到明确错误也不要形成重试风暴。

---

## 配置项

### 环境变量

| 变量 | 说明 |
| --- | --- |
| `WORKBUDDY_TOKEN` | 直接注入 accessToken（优先级高于账号文件，用于临时调试） |
| `WORKBUDDY_REFRESH_TOKEN` | 配合上一个变量使用的刷新凭证 |
| `WORKBUDDY_USER_ID` | 配合使用，作为 `X-User-Id` 头 |
| `WORKBUDDY_ENDPOINT` | 上游端点 |
| `WORKBUDDY_EDITION` | 默认账号版本 |
| `WORKBUDDY_PREFIX_PATH` | 鉴权路径前缀 |
| `WORKBUDDY_DEFAULT_MODEL` | 默认模型 |
| `WORKBUDDY_PROXY_API_KEY` | 网关自身的 API Key |
| `WORKBUDDY_PROXY_HOME` | 配置目录（默认 `~/.workbuddy-proxy`） |
| `WORKBUDDY_DESENSITIZE` | `1` 开启 / `0` 关闭内容脱敏 |
| `WORKBUDDY_LOCALE` | 计费语言 |
| `WORKBUDDY_VERBOSE` | `1` 时输出详细日志（入站请求、上游交互、模型目录刷新等） |
| `WORKBUDDY_PROXY_PORT` | 桌面端使用的网关端口（默认 3065） |
| `WORKBUDDY_SKIP_OPEN_BROWSER` | `1` 时不自动打开系统浏览器（桌面端调试用） |
| `WORKBUDDY_UPDATE_REPO` | 软件更新检测的 GitHub 仓库（默认 `aimod-cc/workbuddy-proxy`） |
| `WORKBUDDY_GITHUB_TOKEN` | 可选。检查更新用的 GitHub token，仅用于提高 API 频率限额（不填也能用；也接受通用的 `GITHUB_TOKEN`，前者优先） |

### 配置文件

| 文件 | 内容 |
| --- | --- |
| `config.json` | 网关 API Key、计费语言、最近一次请求所用模型、自动签到设置（`autoCheckin`）、三档数据保留天数（`logRetentionDays` / `requestRetentionDays` / `dailyRetentionDays`） |
| `accounts.json` | 账号列表（凭证、优先级、启用状态、代理、限额记录） |
| `auth.json` | 旧版单账号登录态（仅在账号列表为空时迁移一次） |
| `desensitize.json` | 脱敏开关、词表、作用角色 |
| `logs.jsonl` | 运行日志（JSONL，一行一条；保留期默认 30 天） |
| `requests.jsonl` | 请求明细（JSONL，一行一次请求；保留期默认 30 天） |
| `request-daily.jsonl` | 按天聚合的用量（一行一天，供报表与热力图；保留期默认 365 天） |
| `desktop-settings.json` | 桌面端设置：关闭到托盘、开机自启 |
| `debug/last-request.json` | 最近一次入站请求体（覆盖写，便于重放） |
| `debug/last-request.meta.txt` | 对应的一行摘要（时间戳、方法、路径、字节数、UA） |
| `updates/` | 软件更新下载的安装包（覆盖写，同版本只保留一份） |

账号文件的读取每次实时走磁盘，手工编辑后下一次请求即生效；`priority` / `enabled` / `proxy` 等字段都可以直接改（优先级仍需保持唯一）。

---

## 数据目录

所有数据都在 `~/.workbuddy-proxy/`（可用 `WORKBUDDY_PROXY_HOME` 覆盖）。Windows 上即 `C:\Users\<你>\.workbuddy-proxy\`。备份或迁移时整个目录拷走即可，其中 `accounts.json` 含明文 token，请勿外传。

```jsonc
// accounts.json 中单条账号的结构（节选）
{
  "id": "user-xxxxxxxx",
  "name": "工作账号",
  "uid": "xxxxxxxx",
  "edition": "cn",                 // cn 国内版 / intl 国际版
  "priority": 100,                 // 转发顺序，全局唯一
  "enabled": true,
  "proxy": null,                   // null 表示直连
  "accessToken": "...",
  "refreshToken": "...",
  "expiresAt": 1789000000000,
  "rateLimits": {                  // 账号 × 模型维度的限额冷却
    "deepseek-v4.1-flash": { "status": 429, "code": 6004, "resetAt": 1789..., "message": "..." }
  }
}
```

---

## 项目结构

网关与桌面端都在 `desktop-tauri/`：后端是 `src-tauri/` 下的 Rust 进程内 HTTP 服务器，前端是 `ui/` 下的原生 HTML/CSS/JS。

```
workbuddy/
├─ desktop-tauri/
│  ├─ src-tauri/src/
│  │  ├─ server/                 网关实现（Rust，进程内 HTTP 服务器）
│  │  │  ├─ mod.rs               服务组装：ServerState、启动与优雅停机
│  │  │  ├─ http.rs              路由表、CORS、API Key 中间件、body 限制
│  │  │  ├─ config.rs / logging.rs / logs_store.rs / errors.rs
│  │  │  ├─ request_stats.rs     请求统计存储（明细 + 按天聚合，供报表页）
│  │  │  ├─ request_stats/       统计的时钟窗口、记录写入、聚合与裁剪
│  │  │  ├─ core/                领域逻辑（按职责分子目录）
│  │  │  │  ├─ endpoints.rs      上游接口清单（唯一事实来源）+ 版本/UA/商品码常量
│  │  │  │  ├─ account_store/    账号存储、优先级、限额记录
│  │  │  │  ├─ account_transfer.rs                账号导入导出（merge 语义）
│  │  │  │  ├─ auth.rs / auth_http.rs / login.rs   会话、出网传输、无头登录
│  │  │  │  ├─ upstream/         请求头复刻、SSE 透传与帧合并、429 轮换、聚合、usage 旁路
│  │  │  │  ├─ models.rs / routing.rs              模型目录、账号选路
│  │  │  │  ├─ billing/          积分、额度、签到、运营活动
│  │  │  │  ├─ proxies.rs / clash.rs / egress.rs   出网代理与按出口缓存 Client
│  │  │  │  ├─ desensitize/      脱敏引擎与词表
│  │  │  │  └─ auto_checkin.rs / update/           定时签到、软件更新
│  │  │  └─ api/                 各路由 handler（health/session/accounts/chat/stats/…）
│  │  ├─ lib.rs                  应用入口：窗口生命周期、插件与命令注册
│  │  ├─ backend.rs              进程内服务器生命周期 + 覆盖升级迁移
│  │  ├─ gateway.rs              壳侧访问管理 API 的 HTTP 客户端
│  │  ├─ login.rs                登录窗口与轮询（含系统浏览器打开）
│  │  ├─ commands.rs             暴露给前端的 invoke 命令
│  │  ├─ bridge.rs               注入 window.workbuddyDesktop 的桥接脚本
│  │  ├─ update.rs               安装包路径校验与启动（更新功能中壳侧的部分）
│  │  └─ settings.rs / state.rs / tray.rs
│  ├─ ui/                        前端（原生 HTML/CSS/JS，无框架）
│  └─ src-tauri/tauri.conf.json  打包配置（NSIS）
├─ build/
│  └─ make-icon.mjs              生成应用图标源图
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

根项目本身没有运行期依赖，`package.json` 只提供上面这些快捷脚本入口。打包产物为 `target/release/bundle/nsis/*.exe`（约 2.4 MB；`src-tauri/.cargo/config.toml` 把 cargo 的 `target-dir` 指到了项目根的 `target/`，所以产物不在 `src-tauri/target/` 下）。

### 关于后端实现

网关是**壳进程内的 Rust HTTP 服务器**（`desktop-tauri/src-tauri/src/server/`），这是项目唯一的后端实现。这样做的好处：

- **安装包小**：不再随包分发外部运行时（旧版曾内置官方 `node.exe`，约 87 MB），安装包从约 24 MB 降到约 2.4 MB。
- **没有子进程**：不存在「升级时杀不掉后端进程导致覆盖安装失败」的问题，退出只是关闭本进程内的监听器。
- **单一实现**：HTTP 契约与磁盘格式只有一处定义，不存在多份实现之间「行为漂移」的维护负担。

> 历史说明：早期版本曾评估过用 Bun 编译或 Node SEA 打成单文件随包分发，都因 SOCKS5 支持缺失（Bun 的 undici shim 不含 `Socks5ProxyAgent`）或注入后二进制签名失效被安全软件误报（Node SEA）而放弃。Rust 重写后这些取舍不再适用。

### 前端桥接

界面代码只依赖 `window.workbuddyDesktop` 这一个接口，由 `bridge.rs` 在页面脚本执行前注入，内部把每个方法映射到 Tauri 的 `invoke`。因此 UI 代码里不出现任何 Tauri 字样——这也是桌面端从 Electron 迁到 Tauri 时前端都没改过的原因。

### 测试

仓库当前**没有自动化测试**：Rust 后端里没有 `#[test]`，也没有 `tests/` 目录。验证方式是构建后手动跑：

```bash
npm run tauri:dev          # 开发模式启动，肉眼验证界面与转发
```

手动验证转发链路时，可用 `WORKBUDDY_PROXY_HOME` 指向一个临时目录，避免影响本机账号数据。

---

## 设计取舍与已知限制

- **模型不做静默回退**：点名不存在的模型直接 400，附带近似名提示。宁可让下游配置报错，也不接受「请求 A 实跑 B」。
- **只实现对话端点**：逆向出的接口清单里还有补全、向量、图像、视频等路径，但都未经实测，本项目没有实现转发（请求会得到 404），模型目录也不收录对应的非对话模型。
- **优先级强制唯一**：这是「主备序号」语义的前提。历史数据里的并列会做一次性去重迁移，相对顺序保持不变。
- **上游只支持流式**：`stream:false` 由网关在内部聚合，代价是拿不到「非流式」的原生行为（例如服务端的 chunk 边界）。聚合时会拼接 `content` / `reasoning_content`、按 `index` 合并 `tool_calls`、取最后一次出现的 `usage`。
- **首条消息必须是 system**：上游要求如此，客户端没带时网关会注入一条兜底系统消息「你是一个得力助手」。
- **签到串行执行**：批量签到一个一个来，避免多账号同时打上游触发风控；代价是账号多时耗时线性增长。
- **自动签到用轮询而非单定时器**：`setTimeout` 在系统休眠、锁屏、时钟被改之后会漂移甚至整段错过，因此改为每 30 秒比对一次「是否已过今天的触发点」，并在启动时补签当天遗漏的一次。判定用「当天日期去重」，与本地时区绑定。
- **国际版无签到活动**：相关操作会被明确跳过（批量）或报错（单个），不会伪装成「签到失败」。
- **代理配置不是「严格镜像」Clash**：账号里只存监听器 uid，端口每次实时读取。Clash 里删了监听器，对应账号回退直连并记日志提醒，不会静默换出口。
- **只监听 `127.0.0.1`，没有改监听地址的入口**：Rust 版把监听地址写死在回环地址上（不再有 Node 版的 `--host`），因此无鉴权时也只有本机进程能访问。若同机存在不受信任的程序，建议在设置页启用 API Key。
- **账号数上限 20 个**，token 长度上限 8192 字符，脱敏词表上限 2000 词。
- **接口形态来自逆向观察**：上游可能随时调整路径、鉴权头或风控策略，届时需要更新 `desktop-tauri/src-tauri/src/server/core/endpoints.rs`（该项目里所有上游接口的唯一事实来源，改这一处即可）。
- **端口被占用时不再「复用已运行的服务」**：旧版桌面端探测到 3065 已有网关就直接复用，这会让管理 API 落到一个版本可能不匹配的外部进程上。现在进程内服务器必须自己绑定成功；若占用者是本产品的旧版进程且位于本应用安装目录内，会自动结束它以完成升级迁移，其余情况给出可操作的错误提示（也可用 `WORKBUDDY_PROXY_PORT` 换端口）。
- **日志有容量与时间两个保留约束**：容量是 500 条的环形保留（管「最多几条」），时间是可配置的保留天数（管「最多留多久」，默认 30 天）。两者同时生效，取先到者；改小天数会把超出的历史记录从内存与文件里一并裁掉。
- **两个视图的分页策略不同，因为数据口径不同**：事件日志一次最多 500 条，因此「不看脱敏」过滤与翻页都在前端做——交互即时，也不会和自动刷新抢状态；副作用是隐藏了多少条只在界面层可见，接口本身不认识这个开关。模型请求明细没有条数上限（一次拉全不现实），走后端 `offset` / `limit` 真分页。
- **请求统计不影响转发链路**：统计所用的 token 用量是旁路提取的，`chat.rs` 的记账包装保证「客户端收到的字节序列与不接统计时完全一致」。

---

## 使用声明

### 仅供学习与交流

本项目是一个用于学习 HTTP 反向代理、SSE 流式透传、多账号选路与桌面端打包（Tauri）等技术主题的实践项目，面向开发者之间的技术交流，**仅供个人学习与研究使用**。它不是官方产品，与腾讯公司及 WorkBuddy / CodeBuddy 官方无任何关联，未获得其授权、认可或赞助。

### 关于反向代理行为

本项目实现的是本地反向代理：在你自己的机器上复用你自己账号的登录态，把请求转发到官方上游网关。它不破解、不绕过任何付费或权限校验，使用的额度始终来自你自己账号本就拥有的配额。

但需要明确的是，这种「以非官方客户端形态复用登录态」的转发方式，**可能不符合上游服务的用户协议或使用条款**。是否使用、由此产生的一切后果（包括但不限于账号被限流、被风控、被冻结或封禁），均由使用者自行承担。

### 禁止用途

禁止将本项目用于任何商业用途、二次分发牟利、批量账号运营、绕过上游计费或配额限制、或其他违反当地法律法规的活动。如需在生产环境或商业场景中调用相关模型服务，请使用官方渠道与官方 API。

### 凭证与数据风险

本项目会把账号凭证（`accessToken` / `refreshToken`）以**明文**形式保存在本机配置目录（默认 `~/.workbuddy-proxy/`）中，导出功能生成的文件同样包含明文凭证。请自行妥善保管，切勿提交到公开仓库、上传到网盘或分享给他人。因凭证泄露造成的损失由使用者自行承担。

### 无担保

本项目按「现状」提供，作者不对其可用性、稳定性、安全性或对特定用途的适用性作任何承诺。上游接口随时可能变更，本项目可能随时失效而不再维护。完整条款见 [LICENSE](./LICENSE)。

### 权利通知

本项目中的接口形态、协议字段等信息来自对官方客户端公开网络通信的观察与整理，相关商标与服务的权利归其各自所有者。若权利方认为本项目存在不当之处，请联系作者，将及时调整或删除。

---

## 许可

本项目基于 [MIT License](./LICENSE) 开源，可自由使用、修改与分发，须保留版权声明。LICENSE 文件末尾附有上述使用声明，两者共同构成完整的授权与使用约定。
