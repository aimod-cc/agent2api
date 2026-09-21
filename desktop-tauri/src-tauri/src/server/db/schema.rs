//! 建表 DDL 与 schema 版本管理（`PRAGMA user_version` 逐版本升级）。
//!
//! ── 为什么自己写十几行而不用 rusqlite_migration ─────────────
//! 迁移库解决的通用问题是「一堆版本文件 + 校验和 + 回滚 + 断点续跑」。本项目
//! 的表**一共 6 张、只增不改**（旧列不删、新列靠可空的 `data` JSON 兜底），
//! 版本推进就是「比大小 → 跑 DDL → 写版本号」三步。引入第三方迁移框架会带来：
//! 一个新依赖、一套它自己的版本表（与 `user_version` 二选一，反而多一份状态）、
//! 以及「框架怎么理解我们的幂等语义」这类需要读源码才能确认的事。十几行确定性
//! 代码可以逐行看懂、出错时能直接对着库文件查，符合本项目一贯取向。
//!
//! ── 为什么全部 DDL 集中在本文件 ─────────────────────────────
//! 表结构是**跨 store 的公共契约**：写侧（各 store 的接线切片）与读侧（API）
//! 分属不同文件、不同任务，两边的字段口径必须有一处唯一的事实来源。字段散在
//! 各 store 的 `CREATE TABLE` 里，加一列就会出现「谁先建谁生效」的库文件差异
//! （同一个版本，A 机器建的库有这列、B 机器没有）。集中之后「库长什么样」只
//! 用读这一个文件。
//!
//! ── 版本推进方式 ────────────────────────────────────────────
//! 读 `user_version`（默认 0 = 全新库），对它到 [`SCHEMA_VERSION`] 之间的
//! 每个版本按序调用 [`apply_version`]，最后把 `user_version` 写成
//! [`SCHEMA_VERSION`]。**每推一个版本都是一个独立事务**（DDL 与版本号一起
//! 提交）：中途失败时已完成的那几个版本保留、最后一个整体回滚，下次启动从
//! 落库的版本号继续 —— 不会出现「表建了一半但版本号已经写新」的错位。
//!
//! ── 为什么所有语句都带 IF NOT EXISTS ────────────────────────
//! 版本号是**唯一**的推进依据，但现实里存在「表已经在了、版本号却是 0」的
//! 情形：用户手工拷过库文件、或者从旧版本二进制回退再前进。DDL 幂等之后这种
//! 库能直接跑过去（缺的补上、有的跳过），而不是在启动时报一句
//! 「table already exists」把用户挡在门外。
//!
//! ── kv 表的键命名规范（后续切片按此约定往里写）──────────────
//! `kv` 是「不属于某张实体表的零散持久状态」的落点，键分两类：
//!   - **网关配置（原 `config.json`）的顶层键直接用键名**，一字不改：
//!     `apiKeys`、`modelRules`、`logRetentionDays`、`debugMode`……
//!     为什么保留原名而不是改成 snake_case：这些名字同时是 **HTTP API 的
//!     契约**（`GET /api/config` 的响应体、前端 `config.X` 的读法），改名会
//!     让前端、账号迁移、模型规则三处同时受影响；数据库的列名与 JSON 键
//!     一一对应，排障时能直接把库里的值贴进配置文件比对。
//!     值的形态也与配置文件一致：**值就是那段 JSON**（标量存 `"30"`、
//!     对象存 `{...}`、数组存 `[...]`），所以 `config.json` → `kv` 的迁移
//!     是「顶层键逐个搬」，不需要为每种类型单独定编码。
//!   - **其它零散状态用固定名**（驼峰，与前面同类）：见 [`RESERVED_KV_KEYS`]。
//!     注意 `desktopSettings` 存的是**整份**设置对象而不是拆成多个键 ——
//!     桌面设置是壳侧 `settings.rs` 的整体读写单元（前端一次 `PUT` 整份），
//!     拆键会让「一次写入」变成多行更新，徒增事务与冲突面。
//!
//! ── 两类键名为什么绝不能相撞（T7 起这是一条硬约束）──────────
//! `config.json` 进了 `kv` 之后，「哪些行属于配置」不再由**表的边界**表达
//! （改造前配置独占一个文件），而是由键名是否落在 [`RESERVED_KV_KEYS`] 里
//! 区分。`config::save_raw` 的「删除已不存在的键」要按这个集合做排除 ——
//! 配置项的名字若与某个固定键撞了，写一次配置就会把那份状态**删掉**
//! （比如配置里出现 `desktopSettings`，用户改一次 API Key 就丢了桌面设置）。
//! 当前配置项（`apiKeys` / `modelRules` / `logRetentionDays` / `debugMode` /
//! `scheduledTasks` / `autoCheckin` / `locale` / `lastRequestModel` /
//! `providerRoute` / 三个 `*Dir` / 三个 `*RetentionDays` / 三个 `retry*` /
//! 六条 `scheduledTasks.*` 子键 / 三个 `*Imported` 标记 / 旧字段 `apiKey`）
//! 与固定键**无冲突**，逐项核对过（全仓 `update_raw_field` / `raw.insert` /
//! `KEY_*` 常量的取值集合 vs 下面的 `RESERVED_KV_KEYS`）。
//! 后加固定键名时**必须**回来核对一次：撞名的代价是用户的配置或状态被静默
//! 覆盖，而两处代码离得很远（一个在本文件，一个在使用方）。

use rusqlite::Connection;

/// `kv` 表里**不属于网关配置**的固定键名（模块头「两类键名绝不能相撞」讲的
/// 就是它与配置顶层键的关系）。
///
/// 为什么要有一份集中清单：`config::save_raw` 必须知道「哪些行不归我管」，
/// 才能在写配置时把「配置里已删掉的键」精确删掉而不误伤别人的状态。没有这份
/// 清单，那个删除只能放宽成「删掉所有不在新配置里的键」—— 桌面设置、账号
/// 优先级作用域、日志下一个 id 会在用户改一次 API Key 时全部消失。
///
/// 它是**事实来源**：各使用方自己的常量（`logs_store::sql::NEXT_ID_KEY`）
/// 仍是它们各自读写时用的名字，本清单只服务
/// 「配置写侧要排除哪些键」这一个判断。两处若不一致，症状是配置写入误删状态
/// —— 所以新增固定键时**两处都要加**（使用方常量 + 本清单），
/// [`is_reserved`] 是唯一的判断入口。
///
/// 清单里可能有**没有使用方常量**的项（如遗留的 `desensitize`）：那些是
/// 「功能已删、数据留着」的键，登记它们的唯一目的就是让配置写入别去动它。
pub const RESERVED_KV_KEYS: &[&str] = &[
    // 账号优先级的全局作用域（core::account_store::sql）
    "priorityScope",
    // 事件日志的下一个 id 水位（logs_store::sql）
    "logsNextId",
    // 整份桌面设置 JSON（壳侧 settings）—— 一个键而不是拆成三个：
    // 桌面设置是整体读写单元（前端一次 PUT 整份），拆键会让一次写入变成
    // 多行更新，徒增事务与冲突面。
    "desktopSettings",
    // **遗留**：旧「敏感词脱敏」的整份状态（词表 / 开关 / 作用角色 / 作用提供商）。
    //
    // 那个功能已随规则集换成硬编码指纹脱敏（`core::sanitize`）而整体删除，
    // 这个键**不再有任何读写方**。仍然登记在这里是有意的：它意味着配置写入
    // 不会顺手把这行删掉 —— 老用户库里那份自定义词表原样留着（想查还能
    // `sqlite3` 看），而不是因为改一次 API Key 就静默消失。真要清理时手工
    // 删这一行即可。
    "desensitize",
    // config.json 迁移的完成标记（db::migrate::config）。为什么配置这一项
    // **需要**标记键而其余项不需要：配置项是开放集合，没法问「配置迁过了
    // 没有」（理由与完整论证见 db::migrate::config 的模块头）。
    "configMigrated",
    // 请求明细 / 聚合的迁移完成标记（request_stats::legacy）。
    //
    // 这两项用标记键，而 `logs` / `accounts` / `debug_traffic` 三项不用，
    // 原因是**那三张表有可用的业务主键**（历史行号 / 记录 uuid / 请求关联 id），
    // 「这条已经导过了」能直接由主键判断（`INSERT OR IGNORE`）；而
    // `requests` 的主键是自增 `row_id`（与内容无关）、`id` 列在旧行里可能是
    // 空串，`request_daily` 的 `date` 又会被运行期 UPSERT 改写 —— 都当不了
    // 「这条已经导过了」的判据。完整论证见
    // `request_stats::legacy::MARKER_REQUESTS` 的文档注释。
    //
    // 登记在这里是**必须**的：`config::save_raw` 写配置时要靠本清单排除
    // 「不归我管」的键，漏登记会让用户改一次 API Key 就把这两个标记删掉
    // —— 下次点「升级」会重复导入一遍历史明细。
    "requestsMigrated",
    "dailyMigrated",
];

/// 这个键是否属于「其它零散状态」（即不归网关配置管）。
///
/// 唯一入口：`config::save_raw` 用它决定哪些行不参与「删除已不存在的键」。
/// 做成函数而不是让调用方自己 `.contains()`：将来若固定键改成带前缀的命名，
/// 改一处即可（与 `db::migrate` 各迁移项包一层 `sql::has_key` 同一手法）。
pub fn is_reserved(key: &str) -> bool {
    RESERVED_KV_KEYS.contains(&key)
}

/// 当前 schema 版本。加表 / 加列时 +1，并在 [`apply_version`] 里补一个分支。
///
/// 为什么从 1 而不是 0 起步：0 是全新库的初始值，用它当「已是最新版」会让
/// 全新库跳过建表。1 起步后「库里什么都没有」与「schema 版本 1」是两件事。
///
/// ── 版本 2：requests 表加两个 JSON 文本列 ──────────────────────
/// 见 [`V2_SCHEMA`]。新库会按 1 → 2 的顺序跑完（v1 建表时**不含**这两列，
/// 由 v2 的 ALTER 加上），所以「全新库」与「从 v1 升上来的库」最终形态一致 ——
/// 这也是为什么 v1 的 DDL **不**回填这两列：那里改了会让两条路径分叉
/// （新库有列、老库经 ALTER 也有列，但列的定义来源变成两处，下次改动容易漏一处）。
pub const SCHEMA_VERSION: i64 = 2;

/// 版本 1 的全部表与索引：改造前所有 JSON / JSONL 文件的对应形态。
///
/// 为什么把六张表写成一个 batch 而不是每张一个常量再拼：它们共同构成「版本 1
/// 的 schema」这一个整体，分开的常量只会让「v1 到底包含什么」需要跨常量拼接
/// 才能看清。单张表的定界用注释块表达（下方按表分块），语句本身用
/// `execute_batch` 一次提交。
const V1_SCHEMA: &str = "
-- ── kv：通用键值表 ────────────────────────────────────────────
-- 存什么：网关配置的每个顶层键、桌面设置整份、各存储的元数据、零散状态。
-- 为什么要有它：这些数据的共同点是「没有固定结构、读写都是整份进出」——
-- 为它们各建一张表，表数量会随配置项增长；塞进一个 TEXT 列则失去按 key 精确
-- 读写的能力（那是它们唯一需要的查询能力）。一列值 + 主键键名正好。
-- 为什么值是 TEXT 而不是 BLOB / JSONB：SQLite 没有原生 JSON 类型，JSON 就是
-- 文本；存 TEXT 时 `json_extract` 等 JSON 函数可以直接用来做条件查询，
-- 而 BLOB 会强制每次比较都做编码转换。值里存什么形态由写入方决定（见模块头
-- 的键命名规范），本表不做校验。
CREATE TABLE IF NOT EXISTS kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ── accounts：账号表（原 accounts.json）────────────────────────
-- 存什么：一条账号记录一行。`data` 列存**整条账号记录的 JSON 原文**。
-- 为什么保留 JSON 原文而不是把每个字段都拆成列：「用户升级绝不能丢账号里
-- 任何字段」是本项目的硬约束（含未知字段、用户手工加的备注、旧版本遗留的
-- currentAccountId），而账号里有 `rateLimits` 这类嵌套结构、各家 provider
-- 的字段集还在演进。拆列意味着每加一个字段都要一次 schema 迁移 + 一次数据
-- 重写，且**漏掉一个字段就等于升级时把那列的数据静默丢掉** —— 这是
-- `core::account_store` 模块头第 1 条不变量，JSON 原文是唯一能自然满足它的形态。
-- 那一列的例外：需要**排序 / 筛选 / 校验唯一性**的字段额外提取成列，
-- 它们的值从 JSON 里复制一份（权威副本仍是 `data`，提取列是查询用的投影）。
--   id       主键，账号唯一标识（与 JSON 里的 id 同值）
--   provider 提供商 id（workbuddy / raccoon / cline-free…），按家分组查
--   priority 优先级数值，越小越先用（全局唯一，见账号存储不变量 2）
--   enabled  是否启用，0/1。SQLite 没有布尔类型，按惯例用 INTEGER
--   added_at 加入时间（毫秒 Unix 时间戳），同优先级时的稳定排序依据
--   data     整条记录的 JSON 原文（**权威副本**）
-- 「已启用且有凭证的第一个账号」这个派生（pick_current）走下面的复合索引：
-- 先按 provider 分片、再按 priority 升序，added_at 保证同优先级顺序稳定。
CREATE TABLE IF NOT EXISTS accounts (
  id       TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  priority INTEGER NOT NULL,
  enabled  INTEGER NOT NULL DEFAULT 1,
  added_at INTEGER NOT NULL DEFAULT 0,
  data     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_accounts_order ON accounts(provider, priority, added_at);

-- ── logs：事件日志（原 logs.jsonl）─────────────────────────────
-- 存什么：日志页展示的运行日志，一行一条。
-- 为什么 id 是 INTEGER PRIMARY KEY 而不另建自增列：SQLite 里
-- 「INTEGER PRIMARY KEY」这一列**就是** rowid 的别名，天然自增、天然唯一，
-- 且是物理存储顺序（按 id 查是 O(log n) 的 B 树查找）。这正是原来内存里
-- 那条「自增 id」的等价物，前端按 id 增量拉取（`since_id`）因此直接可用。
-- 为什么 data 可空：结构化事件的附加字段（429 切换的 from/to 等）只有部分
-- 日志有；空串与「没有 data」语义不同，所以用 NULL 而不是默认 '{}'。
-- 为什么 ts 存 INTEGER 毫秒：原文件里就是 `Date.now()` 的毫秒数，且范围
-- 与负数场景都在 i64 内，不需要 TEXT 的日期格式（排序即数值排序才是重点）。
-- 三个索引（ts / level / category）对应日志页固定的三种过滤维度：
-- 按时间倒序翻页、按「级别及以上」筛选、按分类筛选。刻意**不做**复合索引：
-- 三个维度是独立下拉、组合方式多，复合索引只能覆盖其中一种组合，反而不如
-- 让 SQLite 自己挑（数据量级是几百到几万行，单列索引足够）。
CREATE TABLE IF NOT EXISTS logs (
  id       INTEGER PRIMARY KEY,
  ts       INTEGER NOT NULL,
  level    TEXT NOT NULL,
  category TEXT NOT NULL,
  message  TEXT NOT NULL,
  data     TEXT
);
CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts);
CREATE INDEX IF NOT EXISTS idx_logs_level ON logs(level);
CREATE INDEX IF NOT EXISTS idx_logs_category ON logs(category);

-- ── requests：请求明细（原 requests.jsonl）─────────────────────
-- 存什么：每次对话转发的记账明细（模型、账号、状态、耗时、token 用量）。
-- 为什么是 AUTOINCREMENT 的 row_id 而不是让 id 当主键：
--   row_id 是**自增物理行号**（内部用，保证稳定顺序、可作游标）；
--   id 是请求的**关联键**（与调试报文 debug_traffic.id 同值，靠它把明细与
--   原始报文对上），旧数据里可能是空串，**且不保证唯一**（重试、去重排队
--   的若干路径会产生同 id 的记账）。两个字段职责不同，混用会让「空串 id
--   被当成主键」直接撞唯一约束而丢掉整条记录。
-- 为什么显式写 AUTOINCREMENT：不加它时 rowid 会被复用（删掉最大行后新行
-- 会拿到同一个号），而这里 row_id 会被当作翻页游标，复用会让「翻页漏行 /
-- 重行」。代价是多一张 sqlite_sequence 表，可忽略。
-- 为什么 token 四列各自 DEFAULT 0 而不是可空：原 JSON 里缺失就是 0
-- （前端直接做加法与求和），可空会让每个读点都要处理 NULL；这四个数的
-- 「没有」与「0」在语义上也没有区别。
-- 其余 DEFAULT 同理：都是「旧数据没有这个字段」时的缺省值，含义与文件版
-- 的读取兜底（`Number(x) || 0`）逐字对应 —— 导入旧数据时不必预先补齐每个
-- 字段，缺的列走 DEFAULT 即可。
CREATE TABLE IF NOT EXISTS requests (
  row_id            INTEGER PRIMARY KEY AUTOINCREMENT,
  id                TEXT NOT NULL DEFAULT '',
  ts                INTEGER NOT NULL,
  model             TEXT NOT NULL,
  account_id        TEXT NOT NULL DEFAULT '',
  account_name      TEXT NOT NULL DEFAULT '',
  status            INTEGER NOT NULL DEFAULT 0,
  duration_ms       INTEGER NOT NULL DEFAULT 0,
  first_response_ms INTEGER,
  attempts          INTEGER NOT NULL DEFAULT 1,
  error             TEXT,
  prompt_tokens     INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  total_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  provider          TEXT NOT NULL DEFAULT '',
  client_model      TEXT NOT NULL DEFAULT '',
  upstream_model    TEXT NOT NULL DEFAULT ''
);
-- ts：报表按区间聚合（近 24 小时 / 7 天 / 30 天）与明细按时间倒序翻页。
-- id：由请求 id 反查明细（与调试报文对照、按 id 看一次请求的完整记录）。
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_requests_id ON requests(id);

-- ── request_daily：按天聚合（原 request-daily.jsonl）──────────
-- 存什么：一天一行（date 是**本地时区**的 `YYYY-MM-DD`，与文件名版逐字相同；
-- 时区处理在 `request_stats::clock`，本表只存字符串、不做时区推断）。
-- 为什么 date 直接当主键：一天一行是这张表的全部语义，主键即唯一约束，
-- 天然让「同一天重复记账」变成 UPSERT 而不是插入两行。
-- 为什么三个 *Stats 列是 JSON 数组文本：它们是「当天按模型 / 提供商 / 账号
-- 的累计」，条目数随用户配置变化（模型目录是远程刷新的）、且**整体读写**
-- （报表要整份拿出来算），拆表会让「取一天的三个维度」变成三次 join。
-- 与文件版的行内嵌数组一一对应，导入时原样搬过来即可。
-- 默认 `'[]'` 而不是 NULL：空数组与「这一列没有数据」在这张表里是同一件事，
-- 统一成 `'[]'` 让读取方少一个分支。
-- requests/successful/tokens 这几列是「当天总量」，保留成独立列而不是也从
-- JSON 里算：热力图与汇总行只读它们，拆出来才走得上主键索引。
CREATE TABLE IF NOT EXISTS request_daily (
  date               TEXT PRIMARY KEY,
  requests           INTEGER NOT NULL DEFAULT 0,
  successful         INTEGER NOT NULL DEFAULT 0,
  tokens             INTEGER NOT NULL DEFAULT 0,
  cache_hit_tokens   INTEGER NOT NULL DEFAULT 0,
  cache_input_tokens INTEGER NOT NULL DEFAULT 0,
  model_tokens       TEXT NOT NULL DEFAULT '[]',
  provider_stats     TEXT NOT NULL DEFAULT '[]',
  account_stats      TEXT NOT NULL DEFAULT '[]'
);

-- ── debug_traffic：调试报文（原 debug-traffic.jsonl）──────────
-- 存什么：调试模式下的**上游侧**原始请求/响应（头部已脱敏，脱敏不可关闭）。
-- 为什么 id 是主键：它与 requests.id 同值、是跨表关联键；一条请求只有一份
-- 报文，所以天然唯一（与 requests 不同 —— 那边同 id 可以有多行记账）。
-- 为什么 request/response 的 body 列可空、而 header 列 NOT NULL 带默认值：
-- 请求侧一定存在（我们至少发出去过），响应侧可能因为上游断连而没有；
-- 而头部哪怕没有也应该是 `'{}'`（前端按对象读）。
-- 为什么体用 TEXT 而不用 BLOB：里面是 SSE 文本 / JSON 文本，存 TEXT 才能在
-- `sqlite3` 命令行里直接看（排障场景这是主要用法）。
-- truncated 标记「体被单条上限截断过」（见 debug_traffic::MAX_ENTRY_BYTES），
-- 缺了它前端会把半截响应当完整响应展示。
-- size 是该条**落库后实际占用的字节数**（截断后的三份 JSON 文本 + 两份可空
-- 文本 + 身份字段的长度合计，见 `debug_traffic::sql::payload_bytes`），供两道闸
-- 的字节统计用（`SUM(size)` 与 MAX_TOTAL_BYTES 比较）。
-- 它**不是**「未截断前的原始大小」：闸门守的是「这张表实际占多大」，用截断后的
-- 体积才对得上。这一行原先按「将来可能存原始大小」的设想写成「未截断前」，与
-- 实现不符 —— 改造前的 entry_size 也是截断后体积（原版 record 先 truncate 再算
-- size），所以现在的口径是承接既有实现、不是本次改造新定的。将来真需要原始
-- 大小就**另加一列**（在截断前记一笔），不要复用 size。
-- 索引只有 ts：调试报文按时间倒序翻页是唯一的批量读法，其余都按主键 id 取。
CREATE TABLE IF NOT EXISTS debug_traffic (
  id               TEXT PRIMARY KEY,
  ts               INTEGER NOT NULL,
  url              TEXT NOT NULL DEFAULT '',
  provider         TEXT NOT NULL DEFAULT '',
  request_headers  TEXT NOT NULL DEFAULT '{}',
  request_body     TEXT NOT NULL DEFAULT '',
  status           INTEGER,
  response_headers TEXT,
  response_body    TEXT,
  truncated        INTEGER NOT NULL DEFAULT 0,
  size             INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_debug_traffic_ts ON debug_traffic(ts);
";

/// 版本 2：给 `requests` 补两列 JSON 文本（重试链 + 敏感词命中）。
///
/// ── 这两列解决什么 ──────────────────────────────────────────
/// 请求日志的「重试」列原先只能显示一个「重试」标记（只知道次数），因为
/// 「每一轮换了谁、为什么换」与「这次命中了哪些敏感词」在明细里都没有落点 ——
/// 前者只在转发链路的局部变量里存在过，后者被脱敏模块的聚合统计吃掉。
/// 两列补上之后，那颗标签才有内容可弹（见 `ui/requests-panel.js` 的重试列）。
///
/// ── 为什么是「可空的 JSON 文本」而不是拆表 ─────────────────────
/// 它们是**一条明细的附属历史**，读写都是整份进出、没有按单次尝试查询的
/// 需求（要排障就在 `sqlite3` 里对 JSON 列用 `json_each`）。与 `request_daily`
/// 的三个 `*Stats` 列同一取舍 —— 理由在 `request_stats::record::RequestEntry
/// ::attempt_details` 的注释里写得更细。
///
/// ── 默认值为什么是 `'[]'` 而不是 NULL ─────────────────────────
/// 与 `request_daily` 那三个列一致：空数组与「这一列没有数据」是同一件事，
/// 统一成 `'[]'` 让读取方少一个分支。写入侧（`sql::encode_json_list`）也把
/// 序列化失败兜成 `'[]'`，两边形态一致。
///
/// ── 为什么用 ALTER 而不重建表 ────────────────────────────────
/// 两张表的其它列与索引一个都不动，重建要「建新表 → 搬数据 → 删旧表 → 重建索引」，
/// 而 `requests` 可能有几万行（`MAX_ENTRIES` = 20000）。ALTER ADD COLUMN 是
/// SQLite 的元数据操作（常数时间、不重写数据），代价可以忽略。
///
/// ── 为什么这里不能写 IF NOT EXISTS ───────────────────────────
/// SQLite 的 `ALTER TABLE ... ADD COLUMN` **不支持** `IF NOT EXISTS`（这是它与
/// 本文件所有 CREATE 语句的关键差别）。幂等性因此由**版本号**保证 ——
/// 一个版本只跑一次，跑完立刻把 `user_version` 写在同一个事务里（见
/// `migrate` 的说明）。写这段时不要「顺手加一个 IF NOT EXISTS」：
/// 语句会直接报语法错，而不是变成空操作。
///
/// 另外：全新库会先跑 v1（建出**不含**这两列的 requests 表）再跑本版本，
/// 于是两条路径（新库 / 老库升级）得到同一份表结构。不要为了「少跑一条 ALTER」
/// 把这两列塞回 v1 的 DDL —— 那样老库升级后与 v1 的 DDL 定义就对不上了。
const V2_SCHEMA: &str = "
-- ── requests 补两列（schema v2）───────────────────────────────
-- attempt_details：每次上游尝试的明细 `[{provider,status,error}]`，
--   按发生顺序。条数 <= attempts（采集侧一一对应），另受
--   `core::upstream::usage::MAX_ATTEMPT_DETAILS`（24）的体积闸约束。
-- sensitive_hits：本次请求命中的脱敏规则 `[{word,count}]`，按次数降序。
--   来源是脱敏模块算出的命中明细（改造前是「命中的敏感词」，现在是
--   「命中的硬编码规则标签」，见 `core::sanitize`；列名与结构未动，
--   前端请求日志的「敏」标签因此照常工作）。
-- 两列都 NOT NULL DEFAULT '[]'：见上面「默认值为什么是 '[]'」。
-- 「DEFAULT 后面的值必须是常量」——SQLite 对带非常量默认值的 ADD COLUMN 会
-- 直接报错，`'[]'` 是字面量，满足这条。
ALTER TABLE requests ADD COLUMN attempt_details TEXT NOT NULL DEFAULT '[]';
ALTER TABLE requests ADD COLUMN sensitive_hits TEXT NOT NULL DEFAULT '[]';
";

/// 把库升到 [`SCHEMA_VERSION`]（幂等：已是最新版时什么都不做）。
///
/// 返回 `rusqlite::Result` 而不是本模块自造的字符串错误：调用方 `Db::open`
/// 是唯一消费点，它把错误包成一句人能读的话（`format!`）就够了；而这里需要
/// 区分「读版本号失败 / 跑 DDL 失败 / 写版本号失败」三种，保留 rusqlite 的
/// 错误类型（含 SQLite 的错误码与消息）才说得清是哪一步坏的。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current >= SCHEMA_VERSION {
        // 版本比本二进制新（用户从新版回退到旧版）：**不报错、也不动库**。
        // 报错会让旧版直接起不来（用户只是想回退一次），而「降级 schema」
        // 意味着删列删表，那是不可逆的数据丢失 —— 两者都不该自动发生。
        return Ok(());
    }
    for version in (current + 1)..=SCHEMA_VERSION {
        // 每个版本一个独立事务：DDL 与版本号必须一起生效或一起不生效，
        // 否则中断后会出现「表已建、版本号还是旧的」（下次启动重跑 DDL，
        // 靠 IF NOT EXISTS 能过）与「版本号已新、表没建全」（**跑不过去**）
        // 两种错位，后者更糟，所以宁可牺牲「一次提交全部版本」的原子性。
        //
        // 用 `unchecked_transaction()` 而不是 `transaction()`：后者要
        // `&mut Connection`，而本函数的签名是 `&Connection`（它在 `Db::open`
        // 里被调用，那时 `Db` 还没构造出来，拿不到 `with_mut`）。「同一连接上
        // 不能有嵌套事务」这条约束由调用点保证 —— `Db::open` 持有唯一那把锁，
        // 迁移期间不会有第二个访问者。
        //
        // 注意 `tx` 不需要 `mut`：`pragma_update` / `commit` 都只取 `&self`，
        // 写事务的独占性由 SQLite 自己保证（同一连接上不允许第二条语句并发），
        // 不是靠 Rust 的独占借用。
        let tx = conn.unchecked_transaction()?;
        apply_version(&tx, version)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

/// 执行某个版本的 DDL。**新增版本时在这里加一个分支**，并在上面写好该版本的
/// 建表/改表语句常量。
///
/// `_` 分支返回 Ok 而不是 unreachable!：版本号是外部输入（库文件可能来自
/// 更新的二进制，被旧版读到），越过 [`SCHEMA_VERSION`] 的版本不该 panic。
/// `migrate` 的主循环也不可能走到这里（循环上界就是 SCHEMA_VERSION），
/// 所以这个分支实际是「调用方传了越界的 version」的防御。
fn apply_version(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    match version {
        1 => conn.execute_batch(V1_SCHEMA),
        // v2：requests 补两个 JSON 文本列（重试链 / 敏感词命中）。
        // 与 v1 的差别只有一条：这里是 ALTER 而不是 CREATE，所以**没有**
        // IF NOT EXISTS 可用（见 V2_SCHEMA 的说明），幂等由版本号保证。
        2 => conn.execute_batch(V2_SCHEMA),
        _ => Ok(()),
    }
}
