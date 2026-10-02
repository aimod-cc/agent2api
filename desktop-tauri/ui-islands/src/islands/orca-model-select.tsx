/**
 * Agent2API · OrcaRouter 的**模型下拉**（能力过滤后的真实目录，不是自由文本）。
 *
 * ── 为什么这一家的表单里要多一个模型选择器 ────────────────────
 * 「添加账号」这一步本身不需要模型（账号是一把 Key）。但 OrcaRouter 的模型
 * 清单是**账号级**的（不同 Key 的可见范围不同），而本仓库其余各家的模型管理
 * 都在「模型」页里按家分片、不带能力过滤。OrcaRouter 的目录带两样本仓库其它
 * 家都没有的元数据 —— `supported_endpoint_types` 与
 * `architecture.input_modalities` —— 于是「挑一个能与当前入口匹配的模型」这件
 * 事可以在这一家的表单里就地做实：
 *
 *   · 后端 `GET /api/providers/orcarouter/models?kind=…&modality=…` 用**账号的
 *     Key** 真打一次上游目录，只回最小元数据（**没有任何凭据**）；
 *   · 能力过滤在服务端（未声明能力的模型一律排除，fail closed）；
 *   · 这里把结果渲染成**只能从列表里选**的下拉 —— 不提供自由输入。
 *
 * ── 为什么不是自由文本（硬要求）──────────────────────────────
 * 「让用户自己填模型字符串」会把一个必然失败的值带进请求（拼错、拼对但没有
 * 权限、是图片模型却被拿去对话）。因此本组件只有两个控件：一个能力分段
 * （文本 / 图片理解 / 向量化）和一个下拉；**没有**「手动输入 model」这条路。
 * 目录拉不到时显示后端明确标注的兜底清单（`degraded: true` + 原因），
 * 同样不给自由输入。
 *
 * ── 凭据边界 ────────────────────────────────────────────────
 * 本文件不接触任何 Key：它只请求那条聚合了「服务端持 Key 发请求」的接口，
 * 拿回来的是 `{id, name, maxInputTokens, inputModalities, …}`。
 */

import * as React from 'react'
import { Button, DialogSection, Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@ui'

import { shared } from './add-account-bridge'

/** 后端目录响应里的一条模型（`orcarouter::adapter::catalog_payload`） */
export type OrcaModelOption = {
  id: string
  name?: string
  maxInputTokens?: number
  maxOutputTokens?: number
  supportedEndpointTypes?: string[]
  inputModalities?: string[]
  supportsReasoning?: boolean
  reasoningEfforts?: string[]
}

/** 后端目录响应（`{success, data}` 解包后的 data） */
export type OrcaCatalogPayload = {
  provider?: string
  source?: string
  degraded?: boolean
  catalogSource?: string
  count?: number
  liveCount?: number | null
  models?: OrcaModelOption[]
  note?: string | null
  lastRefreshedAt?: number
  secret_masked?: boolean
}

/**
 * 本表单提供的能力视图。**只列本仓库确实有的入口**：
 *   · `text`        —— 对话 / 智能体（本仓库的核心入口）；
 *   · `multimodal`  —— 带图片附件的对话（同一入口的图片那一档，模态取 image）；
 *   · `embedding`   —— 向量化（目录里声明 `embeddings` 端点类型的那些）。
 * 图片生成 / 视频 / 重排本仓库没有对应入口（本表单也不该提供一个选了没有出口的
 * 选项），因此不在这里列出 —— PR 正文里说明了这三类未覆盖的原因。
 */
export const ORCA_KINDS = [
  { value: 'text', label: '对话 / 智能体', modality: '' },
  { value: 'multimodal', label: '图片理解', modality: 'image' },
  { value: 'embedding', label: '向量化（embedding）', modality: '' },
] as const

type KindValue = (typeof ORCA_KINDS)[number]['value']

/** 一条模型在列表里显示的副标题（上下文窗口 / 模态；都没有就留空） */
function describeModel(model: OrcaModelOption): string {
  const parts: string[] = []
  if (model.maxInputTokens) parts.push(`${Math.round(model.maxInputTokens / 1000)}K 上下文`)
  const modalities = (model.inputModalities || []).filter(item => item && item !== 'text')
  if (modalities.length) parts.push(modalities.join('/'))
  return parts.join(' · ')
}

export function OrcaModelSelect({
  provider,
  visible,
}: {
  provider: string
  visible: boolean
}): React.ReactElement {
  const [kind, setKind] = React.useState<KindValue>('text')
  const [catalog, setCatalog] = React.useState<OrcaCatalogPayload | null>(null)
  const [selected, setSelected] = React.useState('')
  const [status, setStatus] = React.useState<'idle' | 'loading' | 'ready' | 'error'>('idle')
  const [error, setError] = React.useState('')
  /** 递增的请求代：旧响应回来时不能覆盖新一次的目录（切能力 / 重试都算新一代） */
  const generation = React.useRef(0)

  const active = ORCA_KINDS.find(item => item.value === kind) || ORCA_KINDS[0]

  const load = React.useCallback(async (): Promise<void> => {
    const ticket = ++generation.current
    setStatus('loading')
    setError('')
    const query = new URLSearchParams({ kind: active.value })
    if (active.modality) query.set('modality', active.modality)
    try {
      const data = (await shared().wbProviders?.customRequest?.(
        'GET',
        `/api/providers/orcarouter/models?${query.toString()}`,
      )) as OrcaCatalogPayload | null | undefined
      if (ticket !== generation.current) return
      setCatalog(data ?? null)
      setStatus('ready')
    } catch (failure) {
      if (ticket !== generation.current) return
      setCatalog(null)
      setError(failure instanceof Error ? failure.message : String(failure))
      setStatus('error')
    }
  }, [active.value, active.modality])

  // 首次可见时拉一次；切能力时重拉（要求 4：能力变化必须重算下拉）
  React.useEffect(() => {
    if (!visible) return
    void load()
  }, [visible, load])

  const models = catalog?.models || []
  // 已选值不再兼容时**清空**并提示（不能静默留着一个不在列表里的值）
  const incompatible = Boolean(selected) && status === 'ready'
    && !models.some(model => model.id === selected)
  React.useEffect(() => {
    if (incompatible) setSelected('')
  }, [incompatible])

  const degraded = status === 'error' || catalog?.degraded === true

  return (
    <DialogSection hidden={!visible} data-testid={`${provider}-models-section`}>
      <h3>模型（能力过滤后的真实目录）</h3>
      <p>
        目录来自本机网关的 <code>/api/providers/orcarouter/models</code>：它用你配置的
        OrcaRouter API Key 在<b>服务端</b>拉取 <code>GET /v1/models</code>，只把模型元数据
        发到这里（密钥不会下发到浏览器）。列表按当前入口的能力过滤，未声明能力的模型不出现。
      </p>
      <div className='field-row'>
        <Select value={kind} onValueChange={value => setKind((value as KindValue) || 'text')}>
          <SelectTrigger
            className='w-[132px] flex-none'
            data-testid={`${provider}-kind-trigger`}
            aria-label='模型用途'
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {ORCA_KINDS.map(item => (
              <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button variant='outline' onClick={() => { void load() }} disabled={status === 'loading'}>
          {status === 'loading' ? '加载中…' : '刷新目录'}
        </Button>
      </div>
      <div className='field-row'>
        <Select value={selected} onValueChange={value => setSelected(value || '')}>
          {/* 触发器给一个**稳定宽度**：默认 `inline-flex` 会按内容（占位文案）
              量体裁衣，于是浮层宽度也跟着抖；给死宽度后触发器与浮层的位置在
              数据到达前后都是可预期的，自动化才能稳定量到「浮层右缘 = 触发器
              右缘」。 */}
          <SelectTrigger
            className='min-w-[320px] flex-1'
            data-testid={`${provider}-model-trigger`}
            aria-label='OrcaRouter 模型'
          >
            <SelectValue placeholder={status === 'loading' ? '加载目录中…' : '选择一个模型'} />
          </SelectTrigger>
          {/* 浮层宽度**锁死**在触发器宽度上：模型 id（`vendor/model`）比触发器
              里的占位文案长得多，不锁的话浮层会被内容撑宽、右边缘跑到触发器之外
              （自动化按 `trigger_panel_right_delta ≤ 2px` 断言这就是「浮层与触发器
              对齐」的判据）。选项文本由 SelectItem 的 truncate 负责截断。 */}
          <SelectContent
            className='w-[var(--anchor-width)]'
            alignItemWithTrigger={false}
            data-testid={`${provider}-model-panel`}
          >
            {models.map(model => {
              const detail = describeModel(model)
              return (
                <SelectItem key={model.id} value={model.id}>
                  {detail ? `${model.id}（${detail}）` : model.id}
                </SelectItem>
              )
            })}
          </SelectContent>
        </Select>
        <span
          className='detail'
          data-testid={`${provider}-models-status`}
          data-source={catalog?.source || (status === 'error' ? 'error' : 'none')}
          data-degraded={degraded ? 'true' : 'false'}
        >
          {status === 'loading' ? '正在拉取目录…' : null}
          {status === 'error' ? `目录拉取失败：${error}` : null}
          {status === 'ready' && !models.length ? '该用途下没有可用模型（目录为空或未声明能力）' : null}
          {status === 'ready' && models.length
            ? `${models.length} 个模型 · 来源 ${catalog?.source || 'unknown'}`
              + (catalog?.degraded ? `（${catalog?.note || '目录可能已过期'}）` : '')
            : null}
          {incompatible ? '（已选模型不在当前列表里，已清空，请重新选择）' : null}
        </span>
      </div>
    </DialogSection>
  )
}
