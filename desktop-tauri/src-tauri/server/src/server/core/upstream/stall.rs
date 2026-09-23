//! 上游流空闲保护：逐分片的无数据计时（对应 OmniProxy 的 `stallGuard`）。
//!
//! ── 为什么需要它（而不是只靠传输层超时）────────────────────────
//! 传输层的 `read_timeout` 是**后备**（取「等待响应头」与「流空闲」的大者，
//! 见 `core::egress`）—— 它必须取大者才不会成为某个旋钮的隐藏天花板，
//! 于是当「等待响应头」调得比「流空闲」大时，真正判定空闲超时的就是这里。
//!
//! 语义与设置页「流式响应空闲超时」逐字对应：相邻两块数据之间超过设定值
//! 没有新数据即判定连接僵死。**收到新数据即重置计时器**，并且计时器在流
//! 启动时就已武装 —— 「上游接受了请求但迟迟不吐第一个字节」同样被覆盖
//! （等响应头超时只管到响应头为止，它后面还有第一块数据）。
//!
//! ── 触发后的形状 ─────────────────────────────────────────────
//! 产出一个 `io::Error` 错误项并结束。两个消费方各自按既有语义收尾：
//! `ForwardStream` 补「错误帧 + [DONE]」（客户端已收到 200 与部分内容），
//! 聚合器折成 502。文案以 [`IDLE_TIMEOUT_PREFIX`] 开头，`ForwardStream`
//! 据此不加「上游流中断」前缀 —— 它不是中断，是保护性判定（与手动终止
//! `cancellation::MANUAL_TERMINATED` 同一处理）。

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{Future, Stream};

/// 空闲超时错误文案的前缀（见模块头的「触发后的形状」）
pub const IDLE_TIMEOUT_PREFIX: &str = "流式响应空闲超时";

/// 给一条上游字节流装上空闲守卫（`idle` 取设置页「流式响应空闲超时」）。
///
/// 入参 / 出参都是 `BoxStream<Result<Bytes, io::Error>>`：两个调用点
/// （`ForwardStream::new` 与自定义家的翻译流）都已在构造时把 reqwest 错误
/// 描述成文案折进 `io::Error`（见各自的说明），这里只管计时。
pub fn idle_guard(
    inner: BoxStream<'static, Result<Bytes, std::io::Error>>,
    idle: Duration,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(IdleGuard {
        inner,
        idle,
        sleep: Box::pin(tokio::time::sleep(idle)),
        done: false,
    })
}

struct IdleGuard {
    inner: BoxStream<'static, Result<Bytes, std::io::Error>>,
    idle: Duration,
    /// 到点即触发；每收到一块数据就重置（见模块头）
    sleep: Pin<Box<tokio::time::Sleep>>,
    /// 已收尾（超时触发或上游结束）—— 之后的 poll 一律返回 None
    done: bool,
}

impl Stream for IdleGuard {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            // 收到数据：重置计时器再原样下发（先重置后返回，保证这一块数据
            // 的「处理后空闲」也被计入下一轮）
            Poll::Ready(Some(item)) => {
                let deadline = tokio::time::Instant::now() + self.idle;
                self.sleep.as_mut().reset(deadline);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => match self.sleep.as_mut().poll(cx) {
                // 上游静默超过 idle：报错并收尾（只触发一次）
                Poll::Ready(()) => {
                    self.done = true;
                    Poll::Ready(Some(Err(std::io::Error::other(format!(
                        "{IDLE_TIMEOUT_PREFIX}（超过 {} 秒无新数据）",
                        self.idle.as_secs()
                    )))))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}
