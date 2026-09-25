import { createRoot } from 'react-dom/client'

import { SegmentedControl, type SegmentedControlOption } from './segmented-control'

/**
 * 分段控件岛 —— 全站分段筛选的统一入口。
 *
 * 用法（业务侧）：
 *   const island = window.wbSegmented.mount(host, {
 *     options: [{ value: 'all', label: '全部', count: 12 }, ...],
 *     value: current,
 *     ariaLabel: '启用状态',
 *     onChange: next => { ... },
 *   })
 *   island.setValue(next)        // 外部改了值后回灌
 *   island.setOptions(options)   // 计数 / 禁用态变了，或选项增删
 *
 * 职责边界（与 ui/select.js 的「增强而非替换」同一取向）：岛只负责渲染与交互
 * —— 语义、键盘、视觉；档位取值、持久化、拉数据全归业务代码，岛是完全受控的，
 * 自己不留状态，于是「切了之后要做什么」不会分裂成两处。
 */

/** 岛只按字符串处理取值（调用方是普通 JS，没有类型可推）；组件内部仍保留泛型 */
type SegmentedIslandOptions = {
  options: readonly SegmentedControlOption<string>[]
  value: string
  /** 无障碍名，对应容器上的 aria-label */
  ariaLabel: string
  onChange: (value: string) => void
  /** 附加到 `.seg` 容器上的类名（弹窗里的 add-seg 靠它带尺寸覆盖） */
  className?: string
}

/** 岛内投影的状态：全由调用方给，岛自己不派生 */
type IslandState = {
  options: readonly SegmentedControlOption<string>[]
  value: string
  ariaLabel: string
  className?: string
}

/**
 * 挂到宿主元素上，返回供业务代码操作它的句柄。
 *
 * 用完全受控而非「岛内自持状态 + ref 暴露 setter」：状态源只有一个（业务侧的
 * 那个变量），岛只做投影，因此不存在两边值不同步的窗口。每次更新都重渲染一次，
 * React 按同位置同类型复用实例，等价于换 props —— 滑块与过渡状态也因此得以保留。
 */
function mountSegmented(host: HTMLElement, initial: SegmentedIslandOptions) {
  const root = createRoot(host)
  let current: IslandState = {
    options: initial.options,
    value: initial.value,
    ariaLabel: initial.ariaLabel,
    className: initial.className,
  }

  const render = () => {
    root.render(
      <SegmentedControl
        options={current.options}
        value={current.value}
        onValueChange={initial.onChange}
        aria-label={current.ariaLabel}
        className={current.className}
      />
    )
  }
  render()

  return {
    /** 外部改了档位后回灌，避免控件停在旧值上 */
    setValue(value: string) {
      if (value === current.value) return
      current = { ...current, value }
      render()
    },
    /** 选项集合变了（计数、禁用态、增删）时整体替换 */
    setOptions(options: readonly SegmentedControlOption<string>[]) {
      current = { ...current, options }
      render()
    },
    unmount() {
      root.unmount()
    },
  }
}

type SegmentedIsland = ReturnType<typeof mountSegmented>

export { mountSegmented, type SegmentedIsland, type SegmentedIslandOptions }
