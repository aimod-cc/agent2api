/**
 * Agent2API · 「添加账号」弹窗第 1 步：选提供商（账号类型分段 + 搜索 + 卡片网格）。
 *
 * 替换旧 add-provider-forms.js 的 providerCards / cardHtml / logoHtml / cardMeta /
 * renderProviderCards / newCardHtml 与 mountAddProviderUi 里注入的那段骨架。
 *
 * 卡片网格与卡片的类名（.add-provider-grid / .add-provider-card / .add-provider-logo…）
 * 原样保留：那是布局性的成套样式（两列网格 + 固定高度滚动 + 卡片三行排版），
 * 组件库没有对应件，重写成 Tailwind 只会把一份已经调好的像素预算再抄一遍。
 * 控件（分段、搜索框）走组件库。
 */

import * as React from 'react'
import { DialogSection, InputGroup, InputGroupAddon, InputGroupInput, SegmentedControl } from '@ui'

import { accountCountOf, shared } from './add-account-bridge'
import type { CustomProviderRecord } from './add-account-bridge'
import { ADD_SEG_CLASS } from './add-provider-blocks'

/** 第 1 步的账号类型：反代（内置八家）/ 预置 API / 自定义 / 导入 */
export type AccountType = 'proxy' | 'preset' | 'custom' | 'import'

export const TYPE_PROXY: AccountType = 'proxy'
export const TYPE_PRESET: AccountType = 'preset'
export const TYPE_CUSTOM: AccountType = 'custom'
export const TYPE_IMPORT: AccountType = 'import'

/**
 * 「导入」分段是否露出。当前 **false**：这一屏还没做完整，先从界面上收起来。
 *
 * 收的是入口、不是实现 —— 导入面板、后端 `/api/import/cc-switch` 与配套 CSS
 * 全部原样留着（见 add-provider-import.tsx），等这一屏补齐把这里改回 true 即可。
 * 关掉之后：分段不生成 → 到不了 TYPE_IMPORT → 面板不挂载、底部「导入所选」
 * 也不会被点亮，走的就是「用户从没点过这个分段」那条路径。
 */
export const IMPORT_SEGMENT_ENABLED = false

/** 预置家卡片的取值前缀（不是 provider id，只是卡片自己的标记） */
export const PRESET_CARD_PREFIX = 'preset:'
/** 「新建自定义提供商」那张卡片的取值（不是 provider id，只是卡片自己的标记） */
export const NEW_PROVIDER_CARD_ID = '__new__'

/** 分段值归一：四个取值之外的一律按「反代」处理（DOM 被人改坏时的保守落点） */
export function typeValueOf(value: string): AccountType {
  if (value === TYPE_PRESET || value === TYPE_CUSTOM) return value
  if (IMPORT_SEGMENT_ENABLED && value === TYPE_IMPORT) return TYPE_IMPORT
  return TYPE_PROXY
}

/**
 * 内置家的真实图标：`id → assets/providers/<file>.png`，图取自各客户端安装目录
 * 内嵌的图标（与系统里显示的为同一张；AutoClaw 国内 / 国际版、Cline 两种账号、
 * Accio / ZCode 两地各自共用一张 —— 它们本来就是同一个客户端）。
 * 自定义家与没收录图标的家回落到首字母徽章。
 */
const PROVIDER_ICONS: Record<string, string> = {
  workbuddy: 'assets/providers/workbuddy.png',
  raccoon: 'assets/providers/raccoon.png',
  catpaw: 'assets/providers/catpaw.png',
  autoclaw: 'assets/providers/autoclaw.png',
  'autoclaw-intl': 'assets/providers/autoclaw.png',
  qoder: 'assets/providers/qoder.png',
  'cline-free': 'assets/providers/cline.png',
  'cline-pass': 'assets/providers/cline.png',
  accio: 'assets/providers/accio.png',
  'accio-cn': 'assets/providers/accio.png',
  zcode: 'assets/providers/zcode.png',
  'zcode-intl': 'assets/providers/zcode.png',
  codearts: 'assets/providers/codearts.png',
  trae: 'assets/providers/trae.png',
  orcarouter: 'assets/providers/orcarouter.png',
}

type CardItem = {
  id: string
  label: string
  count: number
  preset?: boolean
  custom?: boolean
}

/**
 * 第 1 步的卡片列表数据，按账号类型分三段：
 *   · 反代 —— 内置家来自 providers 摘要（现有八家）；
 *   · 预置 API —— 预置目录的官方与托管端点，点一张卡 = 创建这一家并预填；
 *     **已建过同名家的预置卡不再出现**（那张已建卡就在「自定义」段里）；
 *   · 自定义 —— 已建的自定义家（customList），每张卡是「给这家加账号」的对象。
 * 「手动新建」那张卡不在这个列表里：它不是一家提供商，由渲染处单独插在队首。
 */
function providerCards(accountType: AccountType): CardItem[] {
  const providers = shared().wbProviders
  if (accountType === TYPE_PRESET) {
    const customNames = new Set((providers?.customList?.() || []).map(item => item.name || ''))
    return (shared().wbPresetProviders?.list || [])
      .filter(preset => !customNames.has(preset.name))
      .map(preset => ({
        id: PRESET_CARD_PREFIX + preset.key,
        label: preset.name,
        count: 0,
        preset: true,
      }))
  }
  if (accountType === TYPE_CUSTOM) {
    return (providers?.customList?.() || []).map((provider: CustomProviderRecord) => ({
      id: provider.id,
      label: provider.name || provider.id,
      count: accountCountOf(provider.id),
      custom: true,
    }))
  }
  const list = providers?.all?.() || []
  // 摘要还没到时先放 WorkBuddy 一张：弹窗不能因为一次状态未就绪就空着
  const cards: CardItem[] = list.length
    ? list.map(item => ({
      id: String(item.id || ''),
      label: String(item.label || item.id || ''),
      count: Number(item.count) || 0,
    }))
    : [{ id: 'workbuddy', label: 'WorkBuddy', count: accountCountOf('workbuddy') }]
  // 展示顺序微调：两个 AutoClaw 版本要挨着（两列网格里同处一行）且**国内版在前**
  // —— 摘要给的是注册表顺序，把 Qoder 挪到国内版前面即可
  const from = cards.findIndex(item => item.id === 'qoder')
  const to = cards.findIndex(item => item.id === 'autoclaw')
  if (to >= 0 && from > to) cards.splice(to, 0, cards.splice(from, 1)[0])
  return cards
}

/** 卡片图标：收录过的家出真实图标（预置家问预置目录要），其余用首字母徽章 */
function Logo({ item }: { item: CardItem }): React.ReactElement {
  const icon = item.id.startsWith(PRESET_CARD_PREFIX)
    ? shared().wbPresetProviders?.iconOf?.(item.id.slice(PRESET_CARD_PREFIX.length)) || ''
    : PROVIDER_ICONS[item.id]
  if (icon) {
    return (
      <span className='add-provider-logo has-icon'>
        <img src={icon} alt='' loading='lazy' />
      </span>
    )
  }
  const initial = String(item.label || '?').trim().slice(0, 1).toUpperCase() || '?'
  return <span className='add-provider-logo'>{initial}</span>
}

/**
 * 卡片正文第二行（meta）。三段各说各的动作，徽章不再出现 —— 分段已经把类型
 * 分开了，整段都是同一类，再给每张卡挂一枚「预置 / 自定义」徽章只是噪音。
 */
function cardMeta(item: CardItem): string {
  if (item.preset) return '点开即预填，填 Key 接入'
  if (item.custom) return item.count ? `${item.count} 个账号，点击添加` : '点击添加账号'
  return item.count ? `${item.count} 个账号` : '还没有账号'
}

function ProviderCard({ item, onPick }: { item: CardItem; onPick: (id: string) => void }): React.ReactElement {
  return (
    <button
      type='button'
      className='add-provider-card'
      data-provider={item.id}
      role='option'
      onClick={() => onPick(item.id)}
    >
      <Logo item={item} />
      <span className='add-provider-info'>
        <span className='add-provider-name'>{item.label}</span>
        <span className='add-provider-meta'>{cardMeta(item)}</span>
      </span>
      <span className='add-provider-go'>›</span>
    </button>
  )
}

/**
 * 「新建自定义提供商」卡：落在自定义块上，由那个块切到「新建」模式。
 * 说明文案短到一行（与其它卡片同宽）：「OpenAI / Anthropic 兼容」那层约束留给表单页去讲。
 */
function NewProviderCard({ onPick }: { onPick: (id: string) => void }): React.ReactElement {
  return (
    <button
      type='button'
      className='add-provider-card is-new'
      data-provider={NEW_PROVIDER_CARD_ID}
      role='option'
      onClick={() => onPick(NEW_PROVIDER_CARD_ID)}
    >
      <span className='add-provider-logo is-new'>＋</span>
      <span className='add-provider-info'>
        <span className='add-provider-name'>新建自定义提供商</span>
        <span className='add-provider-meta'>接入一个兼容上游</span>
      </span>
      <span className='add-provider-go'>›</span>
    </button>
  )
}

export type PickStepProps = {
  accountType: AccountType
  onAccountTypeChange: (value: AccountType) => void
  search: string
  onSearchChange: (value: string) => void
  onPick: (id: string) => void
  /** 摘要 / 自定义目录异步到位后的重画信号（值本身不参与渲染） */
  version: number
  /** 导入段：卡片网格与搜索整块收起（导入面板占同一块区域） */
  importing: boolean
  /** 导入面板（分段收起时不存在）：与卡片网格互斥显隐，占同一块位置 */
  importPanel?: React.ReactNode
}

export function PickStep({
  accountType,
  onAccountTypeChange,
  search,
  onSearchChange,
  onPick,
  version,
  importing,
  importPanel,
}: PickStepProps): React.ReactElement {
  const cards = React.useMemo(() => providerCards(accountType), [accountType, version])
  const keyword = search.trim().toLowerCase()
  const hit = cards.filter(item => !keyword || item.label.toLowerCase().includes(keyword))
  // 有关键词时「新建」那张卡收起来 —— 用户在找的是一家
  const newCard = accountType === TYPE_CUSTOM && !keyword

  return (
    // 分段与列表同一块：两者是同一个问题的两面（「给什么形态的上游加账号」→
    // 「给哪一家加」），分两块带边框会让人以为是两个独立步骤
    <DialogSection>
      <h3>选择提供商</h3>
      {/* 分段与搜索同一行：左边选形态，右边是这一屏的过滤器 */}
      <div className='add-pick-row' style={importing ? { display: 'none' } : undefined}>
        <SegmentedControl
          aria-label='账号类型'
          className={ADD_SEG_CLASS}
          options={[
            { value: TYPE_PROXY, label: '反代' },
            { value: TYPE_PRESET, label: '预置 API' },
            { value: TYPE_CUSTOM, label: '自定义' },
            // 「导入」分段暂时收起（见 IMPORT_SEGMENT_ENABLED）：整段不生成
            ...(IMPORT_SEGMENT_ENABLED ? [{ value: TYPE_IMPORT, label: '导入' }] : []),
          ]}
          value={accountType}
          onValueChange={value => onAccountTypeChange(typeValueOf(value))}
        />
        <span id='add-search-wrap' className='add-provider-search ml-auto max-w-[340px] min-w-0 flex-[1_1_200px]'>
          <InputGroup>
            <InputGroupAddon>⌕</InputGroupAddon>
            <InputGroupInput
              id='add-provider-search'
              type='search'
              placeholder='搜索提供商…'
              autoComplete='off'
              aria-label='搜索提供商'
              value={search}
              onChange={event => onSearchChange(event.currentTarget.value)}
            />
          </InputGroup>
        </span>
      </div>
      {/* 导入面板插在卡片网格的位置上（与网格互斥显隐），放在网格之前 ——
          两屏内容同一块区域，滚动位置也就跟着换，不会串 */}
      {importPanel}
      <div
        className='add-provider-grid'
        id='add-provider-grid'
        role='listbox'
        aria-label='选择要添加账号的提供商'
        style={importing ? { display: 'none' } : undefined}
      >
        {newCard ? <NewProviderCard onPick={onPick} /> : null}
        {hit.map(item => <ProviderCard key={item.id} item={item} onPick={onPick} />)}
        {!hit.length && keyword ? (
          <div className='add-provider-empty'>{`没有匹配「${search.trim()}」的提供商`}</div>
        ) : null}
      </div>
    </DialogSection>
  )
}
