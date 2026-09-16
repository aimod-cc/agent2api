/**
 * 定时签到 —— 每天在指定时刻执行一次「全部账号签到」
 *
 * 与 /api/accounts/checkin 用的是同一段签到逻辑（由调用方注入 runCheckin），
 * 因此限额跳过、国际版排除、串行防风的规则完全一致，不存在两套行为。
 *
 * 触发方式用「轮询 + 当天去重」而不是单个 setTimeout：
 *   setTimeout 在系统休眠、锁屏、时钟被改之后会漂移甚至整段错过，
 *   而这里每 30 秒比一次「当前是否已过今天的触发点」，唤醒后自然补上。
 *
 * 补签：启动时若今天的时间点已过且今天还没签过，立刻补一次。
 *   签到的每日额度是按自然日重置的，用户开着自动签到却因为
 *   「00:01 时电脑没开机」而整天没签到，才是真正反直觉的结果。
 *   上游签到接口幂等，重复调用只会返回「已领取」，不会重复扣减或报错。
 */

/** 轮询间隔：30 秒足够精确到分钟，又不会让计时器显得忙 */
const TICK_MS = 30_000;

/** 默认触发时刻：零点一分 */
export const DEFAULT_TIME = '00:01';

export class AutoCheckinConfigError extends Error {
  constructor(message) {
    super(message);
    this.name = 'AutoCheckinConfigError';
    this.statusCode = 400;
  }
}

/** 校验并归一化 HH:MM；返回补零后的文本 */
export function normalizeTime(value) {
  const text = String(value ?? '').trim();
  const matched = text.match(/^(\d{1,2}):(\d{1,2})$/);
  if (!matched) throw new AutoCheckinConfigError('时间格式应为 HH:MM（例如 00:01）');
  const hour = Number(matched[1]);
  const minute = Number(matched[2]);
  if (hour > 23 || minute > 59) throw new AutoCheckinConfigError('时间超出范围（00:00 - 23:59）');
  return `${String(hour).padStart(2, '0')}:${String(minute).padStart(2, '0')}`;
}

/** 本地日期键 YYYY-MM-DD：用于「今天是否已触发」的判定 */
function localDateKey(date = new Date()) {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}

/** 今天的触发时刻（本地时区）；已过则返回 null */
function dueNow(timeText, now = new Date()) {
  const [hour, minute] = normalizeTime(timeText).split(':').map(Number);
  const target = new Date(now);
  target.setHours(hour, minute, 0, 0);
  return now >= target ? target : null;
}

/** 距离下一次触发的毫秒数（用于界面上显示「下次执行」） */
export function msUntilNext(timeText, now = new Date()) {
  const [hour, minute] = normalizeTime(timeText).split(':').map(Number);
  const target = new Date(now);
  target.setHours(hour, minute, 0, 0);
  if (target <= now) target.setDate(target.getDate() + 1);
  return target.getTime() - now.getTime();
}

/**
 * @param {object} options
 * @param {() => object} options.loadConfig  读取后端配置（config.json）
 * @param {(patch: object) => void} options.saveConfig 写回配置
 * @param {() => Promise<object>} options.runCheckin 执行全部账号签到（复用 /api/accounts/checkin 的逻辑）
 */
export function createAutoCheckin({
  loadConfig,
  saveConfig,
  runCheckin,
  log = () => {},
  verbose = () => {},
}) {
  let timer = null;
  let running = false;

  function readState() {
    const config = loadConfig() || {};
    const raw = config.autoCheckin && typeof config.autoCheckin === 'object' ? config.autoCheckin : {};
    let time = DEFAULT_TIME;
    try {
      time = raw.time ? normalizeTime(raw.time) : DEFAULT_TIME;
    } catch { /* 配置被手工改坏：回落到默认时刻，不让调度起不来 */ }
    return {
      enabled: raw.enabled === true,
      time,
      lastFiredDate: typeof raw.lastFiredDate === 'string' ? raw.lastFiredDate : null,
      lastResult: raw.lastResult && typeof raw.lastResult === 'object' ? raw.lastResult : null,
    };
  }

  function writeState(patch) {
    const config = loadConfig() || {};
    const current = config.autoCheckin && typeof config.autoCheckin === 'object' ? config.autoCheckin : {};
    config.autoCheckin = { ...current, ...patch };
    saveConfig(config);
  }

  /** 对外暴露的状态（含界面要显示的「下次执行」） */
  function getState() {
    const state = readState();
    const now = new Date();
    return {
      enabled: state.enabled,
      time: state.time,
      lastFiredDate: state.lastFiredDate,
      lastFiredToday: state.lastFiredDate === localDateKey(now),
      nextRunAt: state.enabled ? now.getTime() + msUntilNext(state.time, now) : null,
      lastResult: state.lastResult,
      running,
    };
  }

  /** 执行一次签到并记录结果；失败只记日志，不影响下一次调度 */
  async function fire(reason) {
    if (running) {
      verbose('[Checkin]', '上一次签到仍在执行，本次跳过');
      return null;
    }
    running = true;
    const today = localDateKey();
    try {
      // 先落日期再执行：即便签到中途进程被杀，也不会在重启后反复补签
      writeState({ lastFiredDate: today });
      log('[Checkin]', `⏰ 定时签到开始（${reason}）`);
      const result = await runCheckin();
      const succeeded = Number(result?.succeeded) || 0;
      const total = Number(result?.total) || 0;
      const skipped = Number(result?.skipped) || 0;
      const failed = (Array.isArray(result?.results) ? result.results : [])
        .filter(item => item?.error)
        .map(item => `${item.name || item.id || '未知账号'}（${item.error}）`);

      const summary = {
        at: Date.now(),
        date: today,
        reason,
        succeeded,
        total,
        skipped,
        failed: failed.slice(0, 5),
        failedCount: failed.length,
      };
      writeState({ lastResult: summary });
      log('[Checkin]',
        `定时签到完成: ${succeeded}/${total} 个账号成功领取`
        + `${skipped ? `，跳过 ${skipped} 个` : ''}`
        + `${failed.length ? `，失败 ${failed.length} 个` : ''}`);
      return summary;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      writeState({
        lastResult: {
          at: Date.now(), date: today, reason,
          succeeded: 0, total: 0, skipped: 0,
          failed: [message], failedCount: 1,
        },
      });
      log('[Checkin]', `❌ 定时签到失败: ${message}`);
      return null;
    } finally {
      running = false;
    }
  }

  /** 轮询回调：到点且今天没签过就执行 */
  async function tick() {
    const state = readState();
    if (!state.enabled) return;
    let due;
    try {
      due = dueNow(state.time);
    } catch {
      return;
    }
    if (!due) return;
    if (state.lastFiredDate === localDateKey()) return;
    await fire('到点触发');
  }

  function schedule() {
    if (timer) return;
    timer = setInterval(() => { void tick(); }, TICK_MS);
    // 调度器不该拖住进程退出：服务被关掉时跟着结束
    timer.unref?.();
  }

  function stop() {
    if (!timer) return;
    clearInterval(timer);
    timer = null;
  }

  /**
   * 保存设置。关闭时停表、开启时起表；
   * 改动触发时刻后不需要重启，下一次 tick 即按新时刻判定。
   */
  function configure({ enabled, time } = {}) {
    const patch = {};
    if (enabled !== undefined) patch.enabled = enabled === true;
    if (time !== undefined) patch.time = normalizeTime(time);
    if (!Object.keys(patch).length) throw new AutoCheckinConfigError('没有需要更新的字段');

    const before = readState();
    writeState(patch);
    const after = readState();
    if (before.enabled !== after.enabled) {
      log('[Checkin]', `定时签到已${after.enabled ? '开启' : '关闭'}（${after.time}）`);
    } else if (before.time !== after.time) {
      log('[Checkin]', `定时签到时间已改为 ${after.time}`);
    }
    if (after.enabled) start();
    return getState();
  }

  /** 立即执行一次（界面上的「立即签到」按钮走这里，与定时触发同一条路径） */
  async function runNow() {
    return fire('手动触发');
  }

  /**
   * 启动调度：开启时立即检查一次——覆盖「启动时今天的时间点已过、
   * 但今天还没签过」的补签场景。
   */
  function start() {
    const state = readState();
    if (!state.enabled) return;
    schedule();
    if (state.lastFiredDate !== localDateKey()) {
      void fire('启动补签');
    } else {
      verbose('[Checkin]', `定时签到已于今日执行，下次 ${state.time}`);
    }
  }

  return { getState, configure, start, stop, runNow, tick };
}
