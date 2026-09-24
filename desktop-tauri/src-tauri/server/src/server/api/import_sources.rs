//! 「从其他工具导入」的扫描 API（当前来源：cc-switch）。
//!
//! - `GET /api/import/cc-switch` → 扫描本机 cc-switch 数据库（`core::import_ccswitch`）
//!
//! 与 custom-providers 的管理接口同一档敏感：它读取**另一个本机应用**的配置
//! 文件并在响应里携带 API Key 明文（导入动作需要），因此挂在 protected 之后。
//! 响应里的 `available: false` 是正常业务形态（没装 cc-switch / 文件不可读），
//! 不是错误 —— 前端按空态渲染，这里不返回 4xx/5xx。

use axum::extract::State;
use axum::response::Response;

use crate::server::core::import_ccswitch;
use crate::server::http::ok_json;
use crate::server::ServerState;

pub async fn scan_cc_switch(State(_state): State<ServerState>) -> Response {
    ok_json(import_ccswitch::scan())
}
