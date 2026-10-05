# feat(lobster): PR-1 LobsterAI（网易有道）provider 最小切片


> **审查说明以 PR 正文为唯一权威**;本文件是草稿存档,与正文不一致时以正文为准。
## 这是什么

PR-1 把 LobsterAI（网易有道）接进 Agent2API 的**账号与注册表层**。改完后：
- 「账号」页 providers 摘要出现 LobsterAI
- 桌面端实时登录态可一键导入（本机 LobsterAI App 已登录时）
- 手动粘贴 accessToken + refreshToken 也能加账号
- `/api/accounts/usage` 摘要能读出 jwt_exp / 凭证可用性

## 不做什么

PR-1 **故意不**接：
- `build_chat_request` 转发（PR-1 显式返回 501：`build_chat_request` 报「等待 PR-2」）
- 模型目录远程刷新（`/api/proxy/v1/models`）
- 额度查询（profile-summary / quota）
- 每日签到（activity slot 三步,接入既有定时签到调度）
- Token 主动刷新与回写 sqlite（JWT 2h 内有效，超期需要重新打开 LobsterAI App 让 App 自带刷新通道续期）

为什么：转发路径涉及面大（OpenAI 兼容层 + SSE 解析 + 模型名前缀去重），单独冲刺风险高。先把账号与注册表这层落地，方便你视觉验收（App 里能看到 LobsterAI provider 与你导入的账号），同时给后续 PR-2 提供干净起点。

## 改动面（8 个代码文件 + 1 份 PR 描述文档）

```
M desktop-tauri/src-tauri/server/src/server/api/accounts.rs       (+10)  分派加 Lobster
M desktop-tauri/src-tauri/server/src/server/core/account_store/mod.rs (+1) 注册 pub mod
M desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs (+1) adapter_for 加 Lobster
M desktop-tauri/src-tauri/server/src/server/core/providers/catalog.rs (+2) catalog 加 Lobster 占位
M desktop-tauri/src-tauri/server/src/server/core/providers/mod.rs    (+7) ProviderKind + PROVIDERS + kind_id + kind_from_id
A desktop-tauri/src-tauri/server/src/server/core/account_store/lobster_accounts.rs (+200) 手动 + 桌面导入
A desktop-tauri/src-tauri/server/src/server/core/providers/lobster/credentials.rs    (+120) SQLite kv.auth_tokens 读取 + JWT 解码
A desktop-tauri/src-tauri/server/src/server/core/providers/lobster/mod.rs           (+100) ProviderAdapter 占位实现
```

## 关键设计

- **注册表追加**：`ProviderKind::Lobster`（末尾，与 Trae 相邻）；`PROVIDERS` 末尾插 `"lobster", "LobsterAI"`；`kind_id` / `kind_from_id` 各加一支。**注册表是事实来源**，加这家未改任何现有分支。
- **ProviderAdapter 占位**：写完 trait 的必填签名（`kind` / `list_models` / `build_chat_request` / `classify_error` / `ensure_access_token` / `refresh_models`）；`list_models` 返回空、`build_chat_request` 返回 501；保证 `adapter_for` 穷举 match 编译通过 + 不破坏现有路径。
- **凭证来源**：桌面端 `~/Library/Application Support/LobsterAI/lobsterai.sqlite` 的 `kv.auth_tokens` 行（accessToken + refreshToken）。PR-1 不缓存、每次读盘（桌面端导入是低频操作够用）；PR-2 加 mtime + TTL。
- **账号 ID**：`lobster-{jwt.exp 或 tokenTail 后 4}`，与 raccoon `user-{userId}`、`trae` 的 sha256 不撞空间（与撞 id 保护已就位）。
- **去重**：撞 id 保护——id 被其它 provider 占用时 409（比较的是 `provider` 字段），与 zcode_accounts 同款。

## 复验清单（按顺序）

```bash
# 1. cargo check（PR-1 范围内零错误）
cd desktop-tauri/src-tauri && cargo check

# 2. 构建 App（macOS 上先在 LobsterAI App 登录）
npm run tauri:build

# 3. 桌面端 import API：
curl -s -X POST http://localhost:3065/api/accounts \
  -H 'Content-Type: application/json' \
  -d '{"provider":"lobster","importDesktop":true}' | python3 -m json.tool

# 4. 手动 add（粘贴 token）
curl -s -X POST http://localhost:3065/api/accounts \
  -H 'Content-Type: application/json' \
  -d '{"provider":"lobster","accessToken":"...","refreshToken":"..."}' | python3 -m json.tool

# 5. 期望失败：chat 转发（PR-1 故意返回 501）
curl -s -X POST http://localhost:3065/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"lobster-anything","messages":[]}' | python3 -m json.tool
# → { "error": "...等待 PR-2..." }
```

## PR-2 已规划但不写在本次

1. `ensure_access_token` 接 `/api/auth/refresh`：单飞 + mtime + 30s TTL 缓存 + 回写 sqlite
2. `build_chat_request`：`POST https://lobsterai-server.youdao.com/api/proxy/v1/chat/completions`，OpenAI 兼容 + 仅流式 SSE，鉴权 `Authorization: Bearer <JWT>`
3. `list_models`：`/api/proxy/v1/models` 远程刷新（OpenClaw 状态文件兜底）；模型名前缀 `lobster-`（避免与网关内同名模型撞车，发送侧剥前缀）
4. `usage_query`：`/api/user/profile-summary` 拉 `creditItems`，归一化（total / used / remaining / expires_at）
5. 每日签到：activity slot → context → action 三步，幂等键 + 51104 视同已领
6. UI 表单文案：「LobsterAI 粘贴登录凭据」分支（accessToken + refreshToken 必填）
7. `implemented_kinds` 把 Lobster 列上

（PR-2 已按此清单完成并另行提交，两个 PR 按顺序独立可审。）

## 价值

LobsterAI（网易有道的 AI 助手客户端）此前在本项目支持的上游家族之外。本 PR-1 把它的账号与注册表层接进网关；PR-2 接完后，聊天转发 + 模型目录 + 积分查询 + 每日签到整套可用，LobsterAI 的订阅积分即可像其它内置提供商一样被网关统一调度。