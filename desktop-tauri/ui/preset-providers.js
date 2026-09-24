/* Agent2API · 自定义提供商的预置目录（preset） */

/**
 * 「添加账号 → 自定义」分段下的预置提供商清单：名称 / 图标 / 默认协议 / Base URL
 * 抄自 9Router（Acankao/9router）的 open-sse/providers/registry 注册表 —— 那边每家
 * 存的是完整端点 URL（如 https://api.openai.com/v1/chat/completions），这里按本网关
 * 自定义提供商的 Base URL 语义折算（chat_completions 拼 {baseUrl}/chat/completions，
 * anthropic 拼 {baseUrl}/v1/messages）。
 *
 * 预置只是**新建自定义提供商表单的预填模板**：选中某一家时把名称 / 协议 / Base URL
 * 填进表单（都可改），提交仍走 POST /api/custom-providers，落库后就是一条普通的
 * 自定义提供商（custom- 前缀），与手动新建的没有任何区别。因此这里不需要知道
 * 「哪家已建过」—— 那是账号页 / 弹窗第 1 步按 customList 现算的事。
 *
 * ── 默认协议怎么取 ─────────────────────────────────────────
 * 按该家在 9Router 注册表里的主力端点格式：主打 Claude Code 的 coding 套餐
 * （GLM Coding、Minimax 两家）官方主推 Anthropic 端点，默认 anthropic；其余家
 * 默认 chat_completions。协议下拉在表单里随时可改，hint 里注明另一端的地址。
 *
 * Command Code 有意不预置：它的端点（/alpha/generate）是私有协议，三种协议都
 * 对不上，预置了也转发不通（见 CLAUDE 会话记录）。
 */
(() => {
  /** 协议值与后端 custom_providers::PROTOCOLS 逐字一致（勿改拼写） */
  const P = { openai: 'chat_completions', anthropic: 'anthropic' };

  /**
   * 预置清单。字段：
   *   key      卡片的稳定标记（data-provider="preset:<key>"），也是图标文件名
   *   name     展示名（预填进「名称」输入框，可改）
   *   icon     assets/providers/ 下的图标文件；没有收录的用首字母徽章
   *   protocol 预填协议（下拉仍可改）
   *   baseUrl  预填基址（按本网关的拼接语义，见文件头）
   *   hint     表单 Base URL 栏下的备注：该家另一端协议的地址 / 站点差异说明
   *   quirks   该家的上游特判（随创建写进提供商记录，转发时生效，见下）
   *
   * ── quirks 的取值（抄自 9Router 各家适配器，后端 customProviders 记录的字段）──
   *   urlSuffix          原样追加到出站 URL 的查询串（GLM / MiniMax 的
   *                      Claude 兼容端点要求 ?beta=true，缺了部分 beta 特性不可用）
   *   headers            合并到默认头上的静态额外头（同名覆盖默认头）：
   *                      · Anthropic-Beta —— 启用 interleaved-thinking 等 beta
   *                        特性，Claude 兼容的 coding 端点按 9Router 同款下发；
   *                      · OpenRouter 的 HTTP-Referer / X-Title —— 它的推荐
   *                        标识头（缺了不影响转发，照抄保持同款行为）。
   *   anthropicToolType  "custom" = 发 anthropic 上游时给每个工具补 type
   *                      （MiniMax 的 Claude 兼容端点拒绝无 type 的工具；
   *                      注意 DeepSeek 恰好相反、拒绝 type:"custom"，而本网关
   *                      翻译器默认输出无 type 形态，两家各按各的特判走）。
   *
   * 其余在 9Router 注册表里出现的特判**有意不搬**（本网关的行为已天然覆盖）：
   * preserveCacheControl（chat 透传原样发请求体）、dropOutputConfig（翻译器
   * 不产出 output_config）、anthropicVersion（恒发 anthropic-version）、
   * reasoningInject / forceStream（思考等级注入与恒 stream 是本网关的既有机制）。
   */
  const ANTHROPIC_BETA_HEADERS = {
    'Anthropic-Beta': 'claude-code-20250219,interleaved-thinking-2025-05-14',
  };

  const PRESET_PROVIDERS = [
    {
      key: 'openai', name: 'OpenAI', icon: 'openai.png',
      protocol: P.openai, baseUrl: 'https://api.openai.com/v1',
    },
    {
      key: 'anthropic', name: 'Anthropic', icon: 'anthropic.png',
      protocol: P.anthropic, baseUrl: 'https://api.anthropic.com',
      quirks: { headers: { ...ANTHROPIC_BETA_HEADERS } },
    },
    {
      key: 'openrouter', name: 'OpenRouter', icon: 'openrouter.png',
      protocol: P.openai, baseUrl: 'https://openrouter.ai/api/v1',
      hint: '一个 Key 用遍多家模型，模型名用「厂商/模型」全称（如 deepseek/deepseek-chat）',
      quirks: {
        headers: { 'HTTP-Referer': 'https://endpoint-proxy.local', 'X-Title': 'Endpoint Proxy' },
      },
    },
    {
      key: 'huggingface', name: 'HuggingFace', icon: 'huggingface.png',
      protocol: P.openai, baseUrl: 'https://router.huggingface.co/v1',
      hint: '走 Inference Providers 路由，Key 在 hf.co/settings/tokens 创建',
    },
    {
      key: 'nvidia', name: 'NVIDIA', icon: 'nvidia.png',
      protocol: P.openai, baseUrl: 'https://integrate.api.nvidia.com/v1',
      hint: 'NVIDIA NIM 托管端点，Key 在 integrate.api.nvidia.com 申请',
    },
    {
      key: 'deepseek', name: 'DeepSeek', icon: 'deepseek.png',
      protocol: P.openai, baseUrl: 'https://api.deepseek.com',
      hint: 'Anthropic 兼容端点在 https://api.deepseek.com/anthropic（协议换 anthropic 时填它）',
    },
    {
      key: 'glm', name: 'GLM Coding', icon: 'glm.png',
      protocol: P.anthropic, baseUrl: 'https://api.z.ai/api/anthropic',
      hint: '国际版 z.ai 的 Coding 套餐；OpenAI 兼容地址为 https://api.z.ai/api/coding/paas/v4',
      quirks: { urlSuffix: '?beta=true', headers: { ...ANTHROPIC_BETA_HEADERS } },
    },
    {
      key: 'glm-cn', name: 'GLM 中国', icon: 'glm-cn.png',
      protocol: P.openai, baseUrl: 'https://open.bigmodel.cn/api/coding/paas/v4',
      hint: '智谱 bigmodel.cn 的 Coding 套餐，Key 在 open.bigmodel.cn/usercenter/apikeys',
    },
    {
      key: 'minimax', name: 'Minimax Coding', icon: 'minimax.png',
      protocol: P.anthropic, baseUrl: 'https://api.minimax.io/anthropic',
      hint: '国际站 Coding 套餐；OpenAI 兼容地址为 https://api.minimax.io/v1',
      quirks: {
        urlSuffix: '?beta=true',
        headers: { ...ANTHROPIC_BETA_HEADERS },
        anthropicToolType: 'custom',
      },
    },
    {
      key: 'minimax-cn', name: 'Minimax 中国', icon: 'minimax-cn.png',
      protocol: P.anthropic, baseUrl: 'https://api.minimaxi.com/anthropic',
      hint: '国内站（minimaxi.com）；OpenAI 兼容地址为 https://api.minimaxi.com/v1',
      quirks: {
        urlSuffix: '?beta=true',
        headers: { ...ANTHROPIC_BETA_HEADERS },
        anthropicToolType: 'custom',
      },
    },
    {
      key: 'siliconflow', name: 'SiliconFlow', icon: 'siliconflow.png',
      protocol: P.openai, baseUrl: 'https://api.siliconflow.com/v1',
      hint: '国内站为 https://api.siliconflow.cn/v1',
    },
    {
      key: 'volcengine-ark', name: '火山方舟', icon: 'volcengine-ark.png',
      protocol: P.openai, baseUrl: 'https://ark.cn-beijing.volces.com/api/coding/v3',
      hint: '火山引擎 Coding 套餐；通用推理接入点为 https://ark.cn-beijing.volces.com/api/v3',
    },
    {
      key: 'alicode', name: 'Alibaba', icon: 'alicode.png',
      protocol: P.openai, baseUrl: 'https://coding.dashscope.aliyuncs.com/v1',
      hint: '阿里云百炼 Coding 套餐（国内站）',
    },
    {
      key: 'alicode-intl', name: 'Alibaba Coding', icon: 'alicode-intl.png',
      protocol: P.openai, baseUrl: 'https://coding-intl.dashscope.aliyuncs.com/v1',
      hint: '阿里云百炼 Coding 套餐（国际站）',
    },
    {
      key: 'alims-intl', name: 'Alibaba Studio', icon: 'alims-intl.png',
      protocol: P.openai, baseUrl: 'https://dashscope-intl.aliyuncs.com/compatible-mode/v1',
      hint: '阿里云百炼模型服务（国际站，OpenAI 兼容模式）',
    },
    {
      key: 'alitp-intl', name: 'Alibaba Token Plan', icon: 'alitp-intl.png',
      protocol: P.openai, baseUrl: 'https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1',
      hint: '阿里云 Token Plan 套餐（国际站）',
    },
    {
      key: 'xiaomi-tokenplan', name: '小米 MiMo (Token Plan)', icon: 'xiaomi-tokenplan.png',
      protocol: P.openai, baseUrl: 'https://token-plan-sgp.xiaomimimo.com/v1',
      hint: '小米 MiMo Token Plan 套餐（新加坡站）',
    },
    {
      key: 'opencode-go', name: 'OpenCode Go', icon: 'opencode-go.png',
      protocol: P.openai, baseUrl: 'https://opencode.ai/zen/go/v1',
      hint: '同地址换 anthropic 协议即走它的 Claude 兼容端点',
    },
    {
      key: 'opencode-zen', name: 'OpenCode Zen', icon: 'opencode-zen.png',
      protocol: P.openai, baseUrl: 'https://opencode.ai/zen/v1',
      hint: '同地址换 anthropic 协议即走它的 Claude 兼容端点',
    },
    {
      key: 'ollama-local', name: 'Ollama（本地）', icon: 'ollama-local.png',
      protocol: P.openai, baseUrl: 'http://localhost:11434/v1',
      hint: '本机 Ollama 的 OpenAI 兼容端点，无需 API Key（鉴权留空即可）',
    },
  ];

  /** 按 key 取一家（找不到给 null，调用方自行回落到手动新建） */
  function presetOf(key) {
    return PRESET_PROVIDERS.find(item => item.key === key) || null;
  }

  /** 图标路径：收录过的家出 assets/providers/<file>，没有返回空串（首字母徽章兜底） */
  function iconOf(key) {
    const preset = presetOf(key);
    return preset?.icon ? `assets/providers/${preset.icon}` : '';
  }

  window.wbPresetProviders = { list: PRESET_PROVIDERS, presetOf, iconOf };
})();
