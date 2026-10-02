#!/usr/bin/env python3
"""OrcaRouter GUI 证据检查（pytest 驱动，供独立验证器运行）。

本模块是**仓库自身的可复现入口**：起服务端二进制 → 用 chromium 打开真实面板 →
走真实登录 → 打开「添加账号」选中 OrcaRouter → 截图 → 逐条断言。

  · orca-evidence/auth-methods.png          两种接入方式并列（粘贴 API Key +
                                            Connect with OrcaRouter），密钥框为 password
  · orca-evidence/text-model-dropdown.png   真实展开的模型下拉（数据来自后端目录接口）
  · orca-evidence/multimodal-model-dropdown.png  「图片理解」档按模态过滤后的下拉

前置：仓库已构建（verification-plan.json 的 setup 里 cargo build 与 ui-islands
的 npm ci + build 都在那一步完成）。本检查只做「起服务 → 真界面截图 → 断言」。

环境变量 ORCAROUTER_API_KEY 必需（仅本检查会拿到）；用来在面板里建一条真账号，
模型目录因此走真实 Bearer 请求。密钥绝不打印、绝不落盘。
"""

import hashlib
import json
import os
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from playwright.sync_api import sync_playwright

REPO = Path(__file__).resolve().parents[2]
EVIDENCE_DIR = REPO / "orca-evidence"
CHROMIUM = "/usr/bin/chromium"
# 服务端二进制的落点取决于构建方式：桌面壳的 `.cargo/config.toml` 把
# target-dir 指到 `desktop-tauri/src-tauri/target`，而直接用 `--manifest-path`
# 在仓库根构建时落 根 `target/`。下面两处都找，两个都不在就明确报错。
_SERVER_BIN_CANDIDATES = (
    REPO / "desktop-tauri/src-tauri/target/debug/agent2api-server",
    REPO / "target/debug/agent2api-server",
)


def _server_bin() -> Path:
    for candidate in _SERVER_BIN_CANDIDATES:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    raise AssertionError(
        "未找到已构建的服务端二进制；先跑 setup 里的 "
        "`cargo build --manifest-path desktop-tauri/src-tauri/server/Cargo.toml`："
        + "，".join(str(path) for path in _SERVER_BIN_CANDIDATES)
    )
VIEWPORT = {"width": 1280, "height": 800}
ADMIN_USER = "orca"
ADMIN_PASSWORD = "orcaprobe123"
ARTIFACTS = (
    "auth-methods.png",
    "text-model-dropdown.png",
    "multimodal-model-dropdown.png",
)

# 前端为下拉面板被测的两段测量：都取自真实的 Base UI 弹层几何/样式。
_PANEL_METRICS = """() => {
  const trigger = document.querySelector('[data-testid="orcarouter-model-trigger"]');
  const panel = document.querySelector('[data-testid="orcarouter-model-panel"]');
  if (!trigger || !panel) return null;
  const t = trigger.getBoundingClientRect();
  const p = panel.getBoundingClientRect();
  const style = getComputedStyle(panel);
  const alpha = value => {
    const m = String(value).match(/rgba?\\(([^)]+)\\)/);
    if (!m) return 1;
    const parts = m[1].split(',').map(s => parseFloat(s));
    return parts.length > 3 ? parts[3] : 1;
  };
  return {
    item_count: panel.querySelectorAll('[data-slot="select-item"]').length,
    opaque_background: alpha(style.backgroundColor) >= 0.99,
    visible_border: parseFloat(style.borderTopWidth) >= 1
      && style.borderTopStyle !== 'none',
    trigger_panel_right_delta: Math.abs(t.right - p.right),
    dropdown_open: panel.getBoundingClientRect().height > 0,
  };
}"""


def _sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(8192), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def _wait_ready(base: str, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{base}/login", timeout=2) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            time.sleep(0.25)
    raise AssertionError(f"服务端未在 {timeout:.0f}s 内就绪：{base}/login")


def _post_json(url: str, payload: dict) -> None:
    # 注册面板管理员；已存在时返回 409，忽略即可。
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=5):
            pass
    except urllib.error.HTTPError:
        pass


@pytest.fixture(scope="module")
def evidence() -> dict:
    """起服务端 → 驱动真实界面 → 写 orca-evidence/manifest.json，返回 manifest。"""
    api_key = os.environ.get("ORCAROUTER_API_KEY", "").strip()
    if not api_key:
        pytest.fail("ORCAROUTER_API_KEY 未设置：GUI 证据检查需要真实 Key 拉取模型目录")
    server_bin = _server_bin()

    EVIDENCE_DIR.mkdir(parents=True, exist_ok=True)
    home = tempfile.mkdtemp(prefix="orca-evidence-")
    port = _free_port()
    base = f"http://127.0.0.1:{port}"

    env = {
        **os.environ,
        "AGENT2API_UI_DIR": str(REPO / "desktop-tauri/ui"),
        "AGENT2API_PROXY_HOME": home,
        "AGENT2API_PROXY_PORT": str(port),
        "AGENT2API_ADMIN_USER": ADMIN_USER,
        "AGENT2API_ADMIN_PASSWORD": ADMIN_PASSWORD,
        "AGENT2API_CAPTCHA_ENABLED": "0",
    }
    server = subprocess.Popen(
        [str(server_bin)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        _wait_ready(base)
        _post_json(
            f"{base}/api/panel/setup",
            {"username": ADMIN_USER, "password": ADMIN_PASSWORD},
        )
        manifest = _capture(base, api_key)
    finally:
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()

    (EVIDENCE_DIR / "manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    return manifest


def _capture(base: str, api_key: str) -> dict:
    """用真实界面采集三张证据图并返回 manifest 字典。"""
    ui: dict = {}
    with sync_playwright() as pw:
        browser = pw.chromium.launch(
            executable_path=CHROMIUM,
            args=["--no-sandbox", "--disable-dev-shm-usage"],
        )
        try:
            page = browser.new_page(viewport=VIEWPORT)

            # ── 走真实登录表单进面板 ────────────────────────────────
            page.goto(f"{base}/login", wait_until="domcontentloaded")
            page.wait_for_selector("#username", timeout=15000)
            page.fill("#username", ADMIN_USER)
            page.fill("#password", ADMIN_PASSWORD)
            page.click("#submit")
            page.wait_for_url(
                lambda url: not url.rstrip("/").endswith("/login"), timeout=20000
            )
            page.wait_for_timeout(1500)

            # ── 确保面板里有一条 OrcaRouter 账号 ────────────────────
            # 目录请求要拿账号的 Key 发 Bearer 请求，因此这一步不能省。
            # Key 只在这一段 JS 里当参数传到本机网关，绝不落盘/打印。
            created = page.evaluate(
                """async (apiKey) => {
                  const list = await (await fetch('/api/accounts')).json();
                  const has = (list?.data?.accounts || []).some(
                    a => a.provider === 'orcarouter');
                  if (has) return { created: false };
                  const response = await fetch('/api/accounts', {
                    method: 'POST',
                    headers: { 'content-type': 'application/json' },
                    body: JSON.stringify(
                      { provider: 'orcarouter', apiKey, name: '证据账号' }),
                  });
                  const payload = await response.json();
                  const account = (payload?.data?.account) || {};
                  return { created: true, id: account.id, tail: account.tokenTail };
                }""",
                api_key,
            )
            ui["account_created"] = bool(created.get("created"))
            page.wait_for_timeout(600)

            # ── 打开「添加账号」弹窗，选中 OrcaRouter ────────────────
            page.evaluate("""() => {
              const api = window.wbAddAccountModal;
              if (api && typeof api.open === 'function') api.open();
            }""")
            page.wait_for_timeout(800)
            picked = page.evaluate("""() => {
              const cards = Array.from(
                document.querySelectorAll('[data-provider="orcarouter"]'));
              if (!cards.length) return false;
              cards[0].click();
              return true;
            }""")
            assert picked, "没有找到 OrcaRouter 卡片"
            page.wait_for_timeout(1200)

            auth = page.locator('[data-testid="orcarouter-auth-methods"]')
            api_key_input = page.locator('[data-testid="orcarouter-apiKey"]')
            connect = page.locator('[data-testid="orcarouter-connect"]')
            auth.wait_for(state="visible", timeout=10000)
            ui["api_key_visible"] = api_key_input.is_visible()
            ui["pkce_visible"] = connect.is_visible()
            ui["secret_masked"] = page.evaluate(
                """() => {
                  const el = document.querySelector(
                    '[data-testid="orcarouter-apiKey"]');
                  return !!el && el.getAttribute('type') === 'password';
                }"""
            )
            ui["controls_enabled"] = page.evaluate(
                """() => {
                  const a = document.querySelector(
                    '[data-testid="orcarouter-apiKey"]');
                  const b = document.querySelector(
                    '[data-testid="orcarouter-connect"]');
                  if (!a || !b) return false;
                  const blocked = n => !!(n.disabled
                    || n.getAttribute('aria-disabled') === 'true');
                  return !blocked(a) && !blocked(b);
                }"""
            )
            ui["connect_label"] = (connect.inner_text() or "").strip()
            page.screenshot(path=str(EVIDENCE_DIR / "auth-methods.png"))

            # ── 展开模型下拉（触发一次真实的目录请求）────────────────
            trigger = page.locator('[data-testid="orcarouter-model-trigger"]')
            trigger.wait_for(state="visible", timeout=10000)
            page.wait_for_function(
                """() => {
                  const el = document.querySelector(
                    '[data-testid="orcarouter-models-status"]');
                  return !!el && el.getAttribute('data-source') === 'live';
                }""",
                timeout=30000,
            )
            trigger.click()
            page.wait_for_timeout(1200)
            panel = page.locator('[data-testid="orcarouter-model-panel"]')
            panel.wait_for(state="visible", timeout=10000)

            metrics = page.evaluate(_PANEL_METRICS)
            assert metrics, "下拉未展开，拿不到测量值"
            ui.update(metrics)
            ui["text_item_count"] = metrics["item_count"]
            page.screenshot(path=str(EVIDENCE_DIR / "text-model-dropdown.png"))

            # ── 多模态下拉：切到「图片理解」档，目录按模态重算 ──────────
            page.keyboard.press("Escape")
            page.wait_for_timeout(400)
            page.locator('[data-testid="orcarouter-kind-trigger"]').click()
            page.wait_for_timeout(500)
            page.locator('[data-slot="select-item"]', has_text="图片理解").first.click()
            page.wait_for_function(
                """() => {
                  const el = document.querySelector(
                    '[data-testid="orcarouter-models-status"]');
                  return !!el && el.getAttribute('data-source') === 'live';
                }""",
                timeout=30000,
            )
            page.wait_for_timeout(600)
            trigger.click()
            page.wait_for_timeout(1000)
            panel.wait_for(state="visible", timeout=10000)

            multi = page.evaluate(_PANEL_METRICS)
            ui["multimodal_item_count"] = (multi or {}).get("item_count", 0)
            ui["multimodal_filter_applied"] = bool(
                multi
                and multi["item_count"] > 0
                and multi["item_count"] < metrics["item_count"]
            )
            if multi and multi["item_count"] > 0:
                page.screenshot(
                    path=str(EVIDENCE_DIR / "multimodal-model-dropdown.png")
                )

            # manifest 的目录字段：真读一次后端接口（同一浏览器会话）
            catalog = page.evaluate(
                """async () => {
                  const response = await fetch(
                    '/api/providers/orcarouter/models?kind=text');
                  return await response.json();
                }"""
            )
            multimodal = page.evaluate(
                """async () => {
                  const response = await fetch(
                    '/api/providers/orcarouter/models?kind=multimodal&modality=image');
                  return await response.json();
                }"""
            )
        finally:
            browser.close()

    data = catalog.get("data") or {}
    multi_data = multimodal.get("data") or {}
    passed = bool(
        ui.get("api_key_visible")
        and ui.get("pkce_visible")
        and ui.get("secret_masked")
        and ui.get("controls_enabled")
        and ui.get("dropdown_open")
        and ui.get("item_count", 0) > 0
        and ui.get("opaque_background")
        and ui.get("visible_border")
        and ui.get("trigger_panel_right_delta", 99) <= 2
        and (
            (multi_data.get("count") or 0) == 0
            or ui.get("multimodal_filter_applied")
        )
    )
    # 共享的 UI 断言（auth-methods 那几张只认前四项，下拉那几张再叠尺寸项）
    ui_common = {
        "api_key_visible": bool(ui.get("api_key_visible")),
        "pkce_visible": bool(ui.get("pkce_visible")),
        "secret_masked": bool(ui.get("secret_masked")),
        "controls_enabled": bool(ui.get("controls_enabled")),
    }
    ui_dropdown = {
        **ui_common,
        "dropdown_open": bool(ui.get("dropdown_open")),
        "opaque_background": bool(ui.get("opaque_background")),
        "visible_border": bool(ui.get("visible_border")),
        "trigger_panel_right_delta": ui.get("trigger_panel_right_delta", 99),
    }
    # 归档侧的字段名是 `kind`（消费方按 kind 取 auth-methods /
    # text-model-dropdown / multimodal-model-dropdown），路径相对本目录。
    screenshot_ui = {
        "auth-methods.png": ui_common,
        "text-model-dropdown.png": {**ui_dropdown, "item_count": ui.get("item_count", 0)},
        "multimodal-model-dropdown.png": {
            **ui_dropdown,
            "item_count": ui.get("multimodal_item_count", 0),
        },
    }
    manifest = {
        "provider": "orcarouter",
        # `automation` 是给证据校验器读的那一坨（evidence.validate）。
        "automation": {
            "framework": "playwright",
            "passed": passed,
            "catalog_source": "https://api.orcarouter.ai/v1/models?capability=chat",
            "catalog_model_count": int((data.get("liveCount") or data.get("count") or 0)),
            "image_model_count": int(multi_data.get("count") or 0),
        },
        "multimodal": bool((multi_data.get("count") or 0) > 0),
        "multimodal_filter_applied": bool(ui.get("multimodal_filter_applied")),
        "ui": ui,
        "artifacts": [
            {
                "kind": str(name).removesuffix(".png"),
                "path": name,
                "sha256": _sha256_of(EVIDENCE_DIR / name),
                "size": (EVIDENCE_DIR / name).stat().st_size,
                "ui": screenshot_ui.get(name, ui_dropdown),
            }
            for name in ARTIFACTS
            if (EVIDENCE_DIR / name).exists()
        ],
        "viewport": VIEWPORT,
    }
    return manifest


def test_auth_methods_present(evidence: dict) -> None:
    """两种接入方式并列可用，且密钥输入框是 password 类型。"""
    ui = evidence["ui"]
    assert ui["api_key_visible"], "API Key 输入框不可见"
    assert ui["pkce_visible"], "Connect with OrcaRouter 按钮不可见"
    assert ui["secret_masked"], "API Key 输入框不是 password 类型"
    assert ui["controls_enabled"], "两种接入方式的控件被禁用"


def test_text_dropdown_real_and_styled(evidence: dict) -> None:
    """模型下拉真实展开、选项来自目录、弹层不透明且带可见边框。"""
    ui = evidence["ui"]
    assert ui["dropdown_open"], "下拉没有展开"
    assert ui["item_count"] > 0, "下拉里没有任何模型选项"
    assert ui["opaque_background"], "弹层背景不是不透明"
    assert ui["visible_border"], "弹层没有可见边框"
    assert ui["trigger_panel_right_delta"] <= 2, "弹层与触发器右边缘未对齐"


def test_multimodal_dropdown_is_filtered(evidence: dict) -> None:
    """「图片理解」档的选项必须严格少于文本档 —— 证明按模态过滤生效。"""
    if not evidence["multimodal"]:
        pytest.skip("该账号目录下没有声明 image 输入的多模态模型")
    assert evidence["multimodal_filter_applied"], (
        "多模态下拉与文本下拉条数相同，未按模态过滤"
    )


def test_catalog_comes_from_live_api(evidence: dict) -> None:
    """目录条数来自后端接口，证明下拉不是自由输入/手写示例。"""
    assert (evidence["automation"].get("catalog_model_count") or 0) > 0, "目录接口没有返回模型"
    assert evidence["automation"]["passed"], "UI 证据聚合断言未通过"
