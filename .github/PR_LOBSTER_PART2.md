# feat(lobster): PR-2 转发流式 + 模型目录 + 额度查询 + 每日签到

> Part 2 of 2, completes the LobsterAI（网易有道）integration.
> 依赖 #PR-1（注册表 + 凭证读取 + 账号导入），两个 PR 按顺序独立可审。

## 这是什么

PR-1 落了 LobsterAI 的账号与注册表层；本 PR 把四条链一次接齐：

1. **推理转发**：`POST {server}/api/proxy/v1/chat/completions`，OpenAI 兼容 body，仅流式 SSE（上游不支持 stream:false——实测 stream:false 也回 SSE，统一按流式发避免歧义，客户端要非流式时由网关聚合层还原）。
2. **模型目录**：远程 `/api/proxy/v1/models` > App 同步给内嵌网关的 openclaw.json > 静态兜底两条，三级回落；10 分钟 TTL + 持久化缓存（实测 26 款全量）。
3. **额度查询**：`/api/user/profile-summary` 主轨（`totalCreditsRemaining` + `creditItems` 批次明细，按剩余量降序），`/api/user/quota` 老口径回落（limit → free → monthly → daily → creditsLimit 五分支，服务端显式 Remaining 优先于 total-used 推导）；60 秒 TTL，401 不缓存走刷新重试。
4. **每日签到**：activity 系统三步链（slot → context → check_in，幂等键 + 配置版本号），服务端 51104（已领）视同幂等成功；接入本仓既有定时签到调度（`CHECKIN_PROVIDERS` 第 6 家），不另起守护线程。

## 模型名前缀

LobsterAI 的裸模型名（`glm-5.3-flash`、`deepseek-v4-flash` 等）与网关内其它上游目录大量撞名，因此对外一律加 `lobster-` 前缀暴露，发送侧剥掉（与 Cline 的 `cline-free/` 前缀同一手法）。上游上新模型时目录刷新即可见，零代码适配。

## 凭证生命周期（承接 PR-1 的 sqlite 读取）

- `POST /api/auth/refresh` 主动续期：JWT 余量 < 120 秒时刷新，401/403 后强制刷新；单飞（复用 `providers::refresh_flight` 原语）防并发。
- 刷新结果回写：桌面端来源回写 App 的 sqlite（事务 + 比较写回——App 重新登录换了 refreshToken 时拒绝覆盖，防止旧凭证把用户登出）；手动账号回写账号记录（比较-再写）。
- 桌面账号的凭证**实时读** sqlite（`live_desktop_credentials` 新分支，mtime + 30s TTL 缓存）：App 自己刷新回写后网关即刻可见。

## 改动面

```
M providers/adapter.rs            implemented_kinds 加 Lobster（目录刷新调度）
M providers/catalog.rs            目录来源判定接真（远程落地即远程来源）
M providers/catalog_cache.rs      SCOPE_LOBSTER
M providers/lobster/mod.rs        适配器真身（转发/分类/ensure/refresh/usage）
M providers/lobster/credentials.rs 单飞刷新 + sqlite 回写 + mtime/TTL 缓存
A providers/lobster/models.rs     三级目录 + 前缀剥离
A providers/lobster/balance.rs    额度双轨 + 归一化
A providers/lobster/checkin.rs    三步签到 + 幂等键
M account_store/lobster_accounts.rs 记录查询 + 凭证回写 + 公开形态
M account_store/store_view.rs     public_account 加 lobster 分派
M account_store/store.rs          live_desktop_credentials 加 lobster 分支
M billing/checkin.rs              checkin_for 加 lobster 分派
M core/auto_checkin.rs            CHECKIN_PROVIDERS 5 → 6
M ui-islands …/add-account-configs.ts   表单配置（修「即将上线」误报）
M ui-islands …/accounts-domain.ts       能力位 usage / checkin / expiry
```

测试 8 个：前缀剥离 / 远程与 openclaw 目录解析 / quota 五分支归一 / profile-summary 批次排序 / JWT 解码与临期判定。

## 复验清单（全部实测通过）

```bash
# 1. 转发（SSE；思考 + 正文都透传，帧内 model 回写客户端请求名）
curl -N -X POST http://localhost:3065/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"lobster-glm-5.3-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}'

# 2. 非流式（上游仅流式，网关聚合成标准 JSON 响应）
curl -X POST http://localhost:3065/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"lobster-deepseek-v4-flash","stream":false,"messages":[…]}'

# 3. 模型目录（26 款）
curl -s http://localhost:3065/v1/models | \
  python3 -c "import json,sys; print([m['id'] for m in json.load(sys.stdin)['data'] if m['id'].startswith('lobster-')])"

# 4. 额度（profile-summary 主轨，available + creditItems 批次明细）
curl -s "http://localhost:3065/api/accounts/usage?id=<lobster-账号id>"

# 5. 签到（幂等：今日已领返回 claimed 状态）
curl -X POST http://localhost:3065/api/accounts/checkin \
  -H 'Content-Type: application/json' -d '{"id":"<lobster-账号id>"}'
```

## 界面

- 账号页：LobsterAI 分组与账号卡（有效期读 JWT exp，毫秒口径）；「余额 / 签到」按钮由能力位点亮。
- 添加账号：手动粘贴 accessToken + refreshToken；桌面端登录态一键导入（macOS，读 App 的 sqlite）。
- 模型管理：LobsterAI 26 款（含上下文窗口 / 图像输入模态），可点名调用 `lobster-<模型名>`。

## 已知边界

- 凭证依赖本机 LobsterAI App 的登录态（macOS）；App 未登录时添加账号会如实报「未找到本机登录态」。
- 上游仅流式：`stream:false` 由网关聚合还原，首字节延迟与流式一致。
- 签到活动「今日」由服务端按活动时区判定（实测 Asia/Shanghai），本地不做日历计算。
