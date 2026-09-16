# WorkBuddy Local Proxy

把腾讯 WorkBuddy 桌面端的登录态包装成本地 **OpenAI 兼容 API 网关**，附带多账号管理、积分查询、每日签到、出网代理、内容脱敏与运行日志，并提供一个开箱即用的 Tauri 桌面端。

装上桌面端（或跑一条 `node server.mjs`）后，任何支持自定义 `base_url` 的 OpenAI 客户端都能以 `http://127.0.0.1:3065/v1` 为端点直接使用 WorkBuddy 的模型额度——不需要 API Key，不需要改客户端源码。

```
OpenAI 客户端 / 任意 SDK
        │  POST /v1/chat/completions   （OpenAI 兼容，SSE）
        ▼
  本网关 server.mjs                     ← 本机 127.0.0.1:3065
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
- [命令行用法](#命令行用法)
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

- 支持 `/v1/chat/completions`、`/v1/completions`、`/v1/embeddings`、`/v1/images/generations`、`/v1/videos/generations`，与 `/v1/models` 模型列表。
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
- 运营 banner 与大使状态查询。

**网络与安全**

- 按账号指定出网代理：可选 Clash Verge 的节点监听器或全局混合端口（端口实时读取，不在本工具里复制一份），也可填自定义 HTTP / SOCKS5 代理。代理解析失败自动回退直连并记日志。
- 出口连通性测试（以「能否连上 WorkBuddy 上游」为判据，而不是能否访问墙外站点），附带出口 IP 查询。
- 敏感词脱敏：命中词内部插入零宽空格（U+200B），人眼与模型读到的不变，但上游后端的关键词匹配会失效，用于化解合规模板被内容审核误拦的问题。默认开启，词表与作用角色可在桌面端维护。

**可观测性**

- 运行日志落到 `logs.jsonl`（环形保留最近 500 条，重启后仍可查），分 `debug/info/warn/error` 四级与 7 个分类，429 自动切换带结构化的「原账号 → 目标账号 + 恢复时间」字段。
- `--verbose` 输出每个入站请求与上游交互的细节，并把最近一次请求体落盘到 `debug/last-request.json` 便于重放分析。

**桌面端**

- Tauri 2 打包，随包内置官方 `node.exe` 与后端 bundle，安装后开箱即用，**无需用户自行安装 Node**。
- 六个页面：概览、账号、网关、脱敏、日志、设置；支持明暗主题、系统托盘、开机自启、关闭窗口最小化到托盘、单实例。

---

## 快速开始

### 方式一：桌面端（推荐）

从 Releases 下载安装包（NSIS，简体中文，按当前用户安装），安装后启动即可：

1. 首次启动会自动拉起本机网关（端口 3065）并打开主窗口。
2. 到「概览」页点「登录 / 添加账号」，选账号版本（国内版 / 国际版）与登录方式（内嵌窗口 / 系统默认浏览器），完成一次官方登录。
3. 把 OpenAI 客户端的 `base_url` 填成 `http://127.0.0.1:3065/v1`，`api_key` 随便填（例如 `sk-local`，未启用鉴权时服务端不校验）。

关闭窗口默认只是最小化到托盘，网关继续在后台转发；要彻底退出请在托盘图标上右键选「退出」。

### 方式二：命令行

需要 Node.js **>= 18.17**（推荐 20 或更高）。

```bash
git clone https://github.com/aimod-cc/workbuddy-proxy.git
cd workbuddy-proxy
npm install

# 1. 登录（打印一个链接，浏览器完成登录后自动保存 token）
npm run login

# 2. 启动网关
npm run serve          # 等价 node server.mjs --port 3065
```

启动后终端会打印接口清单与当前账号路由顺序。验证：

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

**网关不会静默改写模型名**：请求里点名的模型必须在目录中真实存在，否则直接返回 400 并附上近似名提示（`model_not_found`）。这是有意为之——把 `deepseek-v4.1-flash` 悄悄换成别的模型会造成「请求 4.1 实跑 V4」这类难以察觉的事故。需要收窄可选模型时，在 `src/workbuddy-models.mjs` 的 `MODEL_ALLOWLIST` 填入白名单（默认空数组即全量放行）。

---

## HTTP 接口

所有管理接口的响应信封为 `{ success: true, data }` 或 `{ success: false, error }`；OpenAI 兼容接口则严格遵循 OpenAI 的错误结构（`{ error: { message, type, ... } }`）。

### OpenAI 兼容

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/v1/chat/completions` | 对话，SSE 流式透传；`stream:false` 由网关内部聚合 |
| POST | `/v1/completions` | 代码补全 |
| POST | `/v1/embeddings` | 向量 |
| POST | `/v1/images/generations` | 文生图 |
| POST | `/v1/videos/generations` | 视频生成 |
| GET | `/v1/models` | 模型列表（OpenAI `list` 格式，附带 `credits`、上下文长度、能力标记） |

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
| GET | `/api/desensitize` | 脱敏状态（开关、词表、角色、命中统计） |
| POST | `/api/desensitize/enabled` | 开关脱敏 |
| POST | `/api/desensitize/roles` | 设置作用角色 |
| PUT / POST / DELETE | `/api/desensitize/terms` | 全量替换 / 追加 / 删除词 |
| POST | `/api/desensitize/reset` | 恢复默认词表 |
| POST | `/api/desensitize/stats/reset` | 清空命中统计 |
| GET | `/api/logs` | 查询运行日志（`limit` / `level` / `category` / `keyword` / `sinceId`） |
| GET | `/api/logs/stats` | 各级别与分类计数 |
| GET | `/api/logs/download` | 导出 JSONL 附件 |
| DELETE | `/api/logs` | 清空日志 |

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

出网统一走 undici（Node 内置 `fetch` 不接受外部 dispatcher，无法按请求指定代理）。dispatcher 按出口缓存复用（最多 24 个，超出按 LRU 回收），否则每个请求都会新建一条到代理的连接。为了让「能转发就一定能刷 token」，token 续期与计费查询也复用账号自己的出口。

### 内容脱敏

客户端自带的 system 合规模板里有一批「拒绝协助 DoS 攻击 / 漏洞利用开发 / 凭证测试」之类的高频词，这些词本身是安全声明，却容易触发上游后端的关键词审核导致请求被误拦。网关的做法是在命中词内部插入零宽空格：

```
DoS  →  D​oS      （中间是一个 U+200B，人眼与模型读到的不变）
```

默认开启，默认作用于 `system` 与 `user` 两种角色（可扩展到 `assistant` / `tool` / `developer`），内置 31 个默认词，词表上限 2000 词、单词 200 字符。纯 ASCII 词会加词边界，避免 `0day` 命中 `100days`、`XSS` 命中 `XSSRF` 这类误伤。词表持久化在 `~/.workbuddy-proxy/desensitize.json`，命中统计在内存中累计。

### 防重试风暴

客户端请求失败后常对同一请求体快速重试，高频重复请求会触发上游频率风控（`code 11128`）并互相续期。网关做了两层防护：

- **请求合并**：相同 body（sha256 去重键）的请求在代理内排队串行，等待上限 45 秒。
- **风控退避**：命中 `11128` 时按 10 秒 / 25 秒退避重试，最多 2 次。间隔故意拉长，因为拉黑期间的每次重试都会给黑名单续期，宁可让客户端拿到明确错误也不要形成重试风暴。

---

## 命令行用法

```bash
node server.mjs [options]
```

| 参数 | 说明 |
| --- | --- |
| `--port <port>` | 监听端口（默认 3065） |
| `--host <addr>` | 监听地址（默认 127.0.0.1） |
| `--api-key <key>` | 启用网关 API Key 认证（监听非回环地址时建议开启） |
| `--edition <cn\|intl>` | 默认账号版本（多账号时以账号记录为准） |
| `--endpoint <url>` | 覆盖上游端点 |
| `--prefix-path <path>` | 覆盖鉴权路径前缀（国内版 `/plugin`，国际版为空） |
| `--default-model <id>` | 客户端未指定模型时的默认值（默认 `auto`） |
| `--locale <locale>` | 计费接口的 `Accept-Language`（默认 `zh-CN`） |
| `--desensitize` / `--no-desensitize` | 本次运行强制开启 / 关闭脱敏（不写回配置文件） |
| `--login` | 无头登录 |
| `--logout` | 清除本地登录态 |
| `--status` | 查看登录态（未登录时退出码为 1） |
| `--usage` | 查询积分/额度 |
| `--checkin` | 执行签到并打印最新积分 |
| `--endpoints` | 打印已逆向的上游接口清单 |
| `-v, --verbose` | 详细日志（入站请求、上游交互、模型目录刷新等） |
| `-h, --help` | 帮助 |

对应的 npm 快捷脚本：`serve` / `login` / `logout` / `status` / `usage` / `checkin` / `endpoints`。

`--checkin` 的输出形如：

```
  今日已签到 : 否
  连续天数   : 3
  累计天数   : 12
  签到结果   : ✅ 领取成功

  总剩余积分   : 1280
  套餐基础积分 : 1000
  平台奖励积分 : 280
```

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
| `WORKBUDDY_VERBOSE` | `1` 等价于 `--verbose` |
| `WORKBUDDY_PROXY_STANDALONE` | `1` 时不做入口判定直接启动服务（被外部进程管理器拉起时用） |
| `WORKBUDDY_PROXY_PORT` | 桌面端使用的网关端口（默认 3065） |
| `WORKBUDDY_SKIP_OPEN_BROWSER` | `1` 时不自动打开系统浏览器（桌面端调试用） |

### 配置文件

| 文件 | 内容 |
| --- | --- |
| `config.json` | 网关 API Key、计费语言、最近一次请求所用模型 |
| `accounts.json` | 账号列表（凭证、优先级、启用状态、代理、限额记录） |
| `auth.json` | 旧版单账号登录态（仅在账号列表为空时迁移一次） |
| `desensitize.json` | 脱敏开关、词表、作用角色 |
| `logs.jsonl` | 运行日志（JSONL，一行一条） |
| `desktop-settings.json` | 桌面端设置：关闭到托盘、开机自启 |
| `debug/last-request.json` | 最近一次入站请求体（覆盖写，便于重放） |

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

```
workbuddy/
├─ server.mjs                    后端入口：HTTP 服务、路由分发、CLI 子命令
├─ src/
│  ├─ workbuddy-endpoints.mjs    上游接口清单（唯一事实来源）+ 版本/UA/商品码常量
│  ├─ workbuddy-auth.mjs         登录态存储、无头登录、token 自动刷新
│  ├─ workbuddy-account-store.mjs 多账号存储、优先级规则、限额记录
│  ├─ workbuddy-account-transfer.mjs 账号导入/导出（纯逻辑，依赖注入）
│  ├─ workbuddy-account-routes.mjs /api/accounts 与 /api/proxies 路由
│  ├─ workbuddy-routing.mjs      账号选路（优先级 + 限额判定）
│  ├─ workbuddy-upstream-client.mjs 请求头复刻、SSE 透传/聚合、429 降级
│  ├─ workbuddy-models.mjs       模型目录（内置兜底 + /v3/config 远程刷新）
│  ├─ workbuddy-billing.mjs      积分、额度、签到、运营活动
│  ├─ workbuddy-proxy.mjs        出网代理（Clash Verge 同步、dispatcher 缓存、出口测试）
│  ├─ workbuddy-desensitize.mjs  脱敏纯函数与词表管理
│  ├─ workbuddy-desensitize-routes.mjs
│  ├─ workbuddy-logs.mjs / workbuddy-log-routes.mjs 运行日志
│  ├─ workbuddy-cli.mjs          CLI 子命令与启动自检输出
│  └─ workbuddy-banner.mjs       启动横幅
├─ desktop-tauri/
│  ├─ src-tauri/src/
│  │  ├─ lib.rs                  应用入口：窗口生命周期、插件与命令注册
│  │  ├─ backend.rs              后端进程生命周期（拉起/复用/回收 node）
│  │  ├─ gateway.rs              管理 API 的 HTTP 客户端
│  │  ├─ login.rs                登录窗口与轮询
│  │  ├─ commands.rs             暴露给前端的 invoke 命令
│  │  ├─ bridge.rs               注入 window.workbuddyDesktop 的桥接脚本
│  │  ├─ settings.rs / state.rs / tray.rs
│  ├─ ui/                        前端（原生 HTML/CSS/JS，无框架）
│  └─ src-tauri/tauri.conf.json  打包配置（NSIS、资源清单）
├─ build/
│  ├─ build-backend.mjs          esbuild 打包后端 + 复制官方 node.exe
│  └─ make-icon.mjs              生成应用图标源图
└─ tests/                        冒烟测试、HTTP 契约测试、脱敏端到端测试
```

---

## 开发与构建

### 环境要求

- Node.js >= 18.17（后端）
- Rust >= 1.77 与 Tauri 2 工具链（仅桌面端需要；Windows 上还需 WebView2 运行时）

### 常用脚本

```bash
npm install                # 安装后端依赖（undici / yaml / esbuild）

npm run serve              # 启动网关（3065 端口）
npm run login              # 无头登录
npm run endpoints          # 打印上游接口清单

npm run build:backend      # 后端 bundle → desktop-tauri/src-tauri/resources/server.cjs + node.exe
npm run build:icon         # 生成图标源图（改图标设计后执行，再跑 tauri icon）

npm run tauri:install      # 安装桌面端依赖
npm run tauri:dev          # 开发模式调起桌面端（自动热重载）
npm run tauri:build        # 构建安装包（会先执行 build:backend）
```

打包产物为 `desktop-tauri/src-tauri/target/release/bundle/nsis/*.exe`。

### 关于后端打包方式

桌面端不打成单文件可执行，而是「官方未改动的 `node.exe` + esbuild 打包的 `server.cjs`」一起分发，原因是：

- **Bun 编译**：Bun 会把 `import 'undici'` 解析成自带 shim，缺少 `Socks5ProxyAgent`，账号走 SOCKS5 出网会崩；真实 undici 在 Bun 下也加载不了（webidl 不兼容）。
- **Node SEA**：需要把 blob 注入 `node.exe`，注入后二进制签名失效，杀毒软件会直接报「发现可疑程序」，分发给别人同样会被拦。

官方 `node.exe` 未做任何改动、签名完整，不会被误报，代价只是安装包大一些（`node.exe` 约 87 MB，已在 `.gitignore` 中排除）。

### 前端桥接

界面代码只依赖 `window.workbuddyDesktop` 这一个接口，由 `bridge.rs` 在页面脚本执行前注入，内部把每个方法映射到 Tauri 的 `invoke`。因此 UI 代码里不出现任何 Tauri 字样——这也是桌面端从 Electron 迁到 Tauri 时前端一行未改的原因。

### 测试

```bash
npm test                              # 冒烟测试：请求头复刻、SSE 聚合、脱敏纯函数、账号存储
node tests/http-test.mjs              # HTTP 契约测试：起真实 server 进程 + 假账号，验证各路由
node tests/desensitize-e2e.mjs        # 端到端：假上游 + 真实网关，验证出站请求体确实被改写
```

三个脚本都用临时配置目录（`WORKBUDDY_PROXY_HOME` 指向 mkdtemp），不会碰你本机的账号数据，也不会访问真实上游。

---

## 设计取舍与已知限制

- **模型不做静默回退**：点名不存在的模型直接 400，附带近似名提示。宁可让下游配置报错，也不接受「请求 A 实跑 B」。
- **优先级强制唯一**：这是「主备序号」语义的前提。历史数据里的并列会做一次性去重迁移，相对顺序保持不变。
- **上游只支持流式**：`stream:false` 由网关在内部聚合，代价是拿不到「非流式」的原生行为（例如服务端的 chunk 边界）。聚合时会拼接 `content` / `reasoning_content`、按 `index` 合并 `tool_calls`、取最后一次出现的 `usage`。
- **首条消息必须是 system**：上游要求如此，客户端没带时网关会注入一条兜底系统消息「你是一个得力助手」。
- **签到串行执行**：批量签到一个一个来，避免多账号同时打上游触发风控；代价是账号多时耗时线性增长。
- **国际版无签到活动**：相关操作会被明确跳过（批量）或报错（单个），不会伪装成「签到失败」。
- **代理配置不是「严格镜像」Clash**：账号里只存监听器 uid，端口每次实时读取。Clash 里删了监听器，对应账号回退直连并记日志提醒，不会静默换出口。
- **无鉴权时假设仅本机可访问**：默认监听 `127.0.0.1`；若改为监听非回环地址，启动时会打印安全提醒，此时应当设置 API Key。
- **账号数上限 20 个**，token 长度上限 8192 字符，脱敏词表上限 2000 词。
- **接口形态来自逆向观察**：上游可能随时调整路径、鉴权头或风控策略，届时需要更新 `src/workbuddy-endpoints.mjs`（该项目里所有上游接口的唯一事实来源，改这一处即可）。

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
