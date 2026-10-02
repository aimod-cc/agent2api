/* Agent2API · web-login.js 的登录生命周期测试（node --test）

   聚焦规范里最难手工复现的一条：**bfcache（pagehide）**。
   浏览器把页面放进 bfcache 后，恢复时**不会重新挂载**组件，于是引擎里那句
   「过期响应不得改动状态」的代次守卫会正确地拒绝复位 —— 若不额外做同步复位，
   按钮就永久卡在忙碌态，且点不动「Connect with OrcaRouter」。

   这里用 node:vm 把 web-login.js 放进一个最小 DOM 桩里跑：它是经典 script，
   依赖 window.wbApp / window.workbuddyDesktop / document.getElementById 与
   全局的 busy / releaseBusy。测试只断言本模块自己的行为，不碰真实网络。 */

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import vm from 'node:vm'

const here = dirname(fileURLToPath(import.meta.url))
const source = readFileSync(join(here, 'web-login.js'), 'utf8')

/** 建一个最小可用的浏览器桩，载入引擎并返回句柄。 */
function bootstrap() {
  const elements = new Map()
  const element = id => {
    if (!elements.has(id)) {
      // innerHTML 与 textContent 共用同一份内容：真实 DOM 里给 textContent
      // 赋值会清掉子节点，这里必须照此模拟，否则「按钮是否还在转圈」测不准
      let html = ''
      elements.set(id, {
        id,
        disabled: false,
        hidden: false,
        get innerHTML() {
          return html
        },
        set innerHTML(value) {
          html = value
        },
        get textContent() {
          return html
        },
        set textContent(value) {
          html = value
        },
      })
    }
    return elements.get(id)
  }
  const listeners = {}
  const calls = { cancel: [], toast: [], onSuccess: 0, resolvers: [], startCount: 0 }

  const sandbox = {
    console,
    Promise,
    Math,
    Date,
    setTimeout,
    clearTimeout,
    busy: false,
    releaseBusy() {
      sandbox.busy = false
    },
    // 组件库的 Button 在真实页面里始终存在；测试里按需自动建，简化断言
    document: { getElementById: id => element(id) },
    addEventListener: (type, handler) => {
      listeners[type] = handler
    },
  }
  // 浏览器里 window 就是全局对象；引擎按 window.xxx 访问依赖
  sandbox.window = sandbox
  sandbox.wbApp = { toast: (message, type) => calls.toast.push([message, type]) }
  sandbox.wbProviders = { labelOf: id => id }
  sandbox.workbuddyDesktop = {
    cancelLogin: options => {
      calls.cancel.push(options)
      return Promise.resolve(true)
    },
    getLoginState: () => Promise.resolve({ active: false, provider: '' }),
    onLoginState: handler => {
      listeners.loginState = handler
    },
  }

  vm.createContext(sandbox)
  vm.runInContext(source, sandbox)

  return { sandbox, element, elements, listeners, calls }
}

/** 建一个 OrcaRouter 形态的控制器；start() 的落点收进 calls.resolvers。 */
function makeController(hands, overrides = {}) {
  return hands.sandbox.window.wbWebLogin.create({
    provider: 'orcarouter',
    buttonId: 'orcarouter-web-button',
    cancelId: 'orcarouter-web-cancel',
    hintId: 'orcarouter-web-hint',
    busyText: '等待 OrcaRouter 授权完成…',
    texts: () => ({ button: 'Connect with OrcaRouter', hint: '浏览器打开授权页' }),
    start: () =>
      new Promise((resolve, reject) => {
        hands.calls.startCount += 1
        hands.calls.resolvers.push({ resolve, reject })
      }),
    onSuccess: () => {
      hands.calls.onSuccess += 1
      return Promise.resolve()
    },
    ...overrides,
  })
}

const flush = async () => {
  for (let i = 0; i < 6; i += 1) await Promise.resolve()
}

test('pagehide 同步复位忙碌态、带 keepalive 取消服务端任务，且不 remount 也能开始第二次登录', async () => {
  const hands = bootstrap()
  const { sandbox, element, listeners, calls } = hands
  const controller = makeController(hands)

  assert.equal(typeof sandbox.window.wbWebLogin.handlePageHide, 'function')
  assert.equal(typeof listeners.pagehide, 'function', '引擎必须监听 pagehide')

  const first = controller.start()
  await flush()
  assert.equal(calls.startCount, 1)
  assert.equal(sandbox.busy, true, '第一次登录应占住弹窗级 busy 锁')
  assert.equal(element('orcarouter-web-button').disabled, true)
  assert.match(element('orcarouter-web-button').innerHTML, /spinner/)

  // 交付 URL 之后用户把页面放进 bfcache —— 恢复时组件不会重新挂载
  listeners.pagehide()

  assert.equal(sandbox.busy, false, 'pagehide 必须同步清掉 busy')
  assert.equal(element('orcarouter-web-button').disabled, false, '按钮必须恢复可点')
  assert.doesNotMatch(element('orcarouter-web-button').innerHTML, /spinner/)
  assert.equal(element('orcarouter-web-hint').textContent, '浏览器打开授权页', 'hint 必须同步复位')
  assert.equal(calls.cancel.length, 1, '必须通知服务端取消在途登录')
  assert.equal(calls.cancel[0]?.keepalive, true, '取消请求必须带 keepalive')

  // 不 remount，直接在恢复后的页面上发起第二次登录
  const second = controller.start()
  await flush()
  assert.equal(calls.startCount, 2, '第二次登录必须真的发起')
  assert.equal(sandbox.busy, true)

  // 迟到的是**第一次**的响应：它属于旧代次，必须被丢弃
  calls.resolvers[0].resolve({ ok: true })
  await flush()
  assert.equal(calls.onSuccess, 0, 'pagehide 之后到达的旧响应不得计入成功')

  // 第二次的正常返回仍然生效
  calls.resolvers[1].resolve({ ok: true })
  await second
  assert.equal(calls.onSuccess, 1)
  assert.equal(sandbox.busy, false, '流程结束后释放 busy 锁')

  first.catch(() => {})
  assert.equal(controller.provider, 'orcarouter')
})

test('pagehide 之后到达的失败响应不得污染界面（不弹错误 toast、不改按钮）', async () => {
  const hands = bootstrap()
  const { sandbox, element, listeners, calls } = hands
  const controller = makeController(hands)

  const pending = controller.start()
  await flush()
  assert.equal(calls.startCount, 1)

  listeners.pagehide()
  calls.resolvers[0].reject(new Error('grant denied long after pagehide'))
  await flush()

  assert.equal(calls.toast.length, 0, '过期失败不得弹错误提示')
  assert.equal(element('orcarouter-web-button').disabled, false)
  assert.equal(sandbox.busy, false)
  await pending.catch(() => {})
})

test('主进程推送的登录状态落到对应 provider 的控制器上（等待中禁用、结束后复位）', async () => {
  const hands = bootstrap()
  const { element, listeners } = hands
  makeController(hands)

  listeners.loginState({ active: true, provider: 'orcarouter' })
  assert.equal(element('orcarouter-web-button').disabled, true)
  assert.equal(element('orcarouter-web-cancel').hidden, false)

  listeners.loginState({ active: true, provider: 'workbuddy' })
  assert.equal(element('orcarouter-web-button').disabled, true, '别家在等待时本家也必须禁用')
  assert.equal(element('orcarouter-web-cancel').hidden, true)

  listeners.loginState({ active: false, provider: '' })
  assert.equal(element('orcarouter-web-button').disabled, false)
  assert.equal(element('orcarouter-web-button').textContent, 'Connect with OrcaRouter')
})
