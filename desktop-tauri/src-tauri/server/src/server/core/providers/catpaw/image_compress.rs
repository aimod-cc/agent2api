//! CatPaw 内联图片压缩：把 round 请求体里的 Base64 大图压成小 JPEG。
//!
//! ── 为什么必须有它（移植来源 `D:\APP\CatPaw\catpaw-local-proxy\image-compress.mjs`）
//! CatPaw 上游**不拉取 http(s) 图片**，图片只能以 `data:image/...;base64,` 的
//! 形式内联进消息；而大图是 round 请求体超限 / 超时的主因（原项目 52862 号
//! 线上问题的根因：单张 Base64 截图约 176KB，round 请求体超过上游约 200KB 的
//! 限制直接 504）。因此请求构造前要把超阈值的内联图压缩。
//!
//! ── 压缩语义（与原实现逐字对照，参数唯一事实来源是上面那个 .mjs）────
//!
//! | JS 原实现               | 本模块                    | 语义                    |
//! |------------------------|--------------------------|-------------------------|
//! | COMPRESS_THRESHOLD_BASE64 = 60*1024 | 同名常量, 60*1024 | **base64 文本长度**超限才压 |
//! | TARGET_OUTPUT_BYTES = 120*1024      | 同名常量, 120*1024 | **JPEG 二进制长度**达标即停 |
//! | MAX_DIMENSION = 1568   | 同名常量, 1568            | 最长边上限（inside 缩放）|
//! | MIN_DIMENSION = 896    | 同名常量, 896             | 降尺寸下限              |
//! | JPEG_QUALITY = 80      | 同名常量, 80              | 首轮质量                |
//! | MIN_QUALITY = 55       | 同名常量, 55              | 质量下限                |
//! | MAX_ATTEMPTS = 4       | 同名常量, 4               | 最多编码 4 次           |
//!
//! 两个阈值**量纲不同**（触发按 base64 字符数、达标按 JPEG 字节数），这是原
//! 实现就有的口径，本次移植照抄不改：60KB 触发 → 压到 ≤120KB 二进制（约
//! 160KB base64）。看起来「压完仍超触发线」，但那是上游请求体限制与本地触发
//! 线的差值，改口径属于行为变更，不在本任务范围。
//!
//! 循环退出条件（逐字移植）：
//!   for attempt in 0..4:
//!     输出 = JPEG(resize(输入, dimension, fit=inside, 不放大), quality)
//!     若 输出.len() <= TARGET_OUTPUT_BYTES → break
//!     quality  = max(55, quality - 15)          // 每轮都降，最低 55
//!     若 attempt >= 1: dimension = max(896, dimension * 3/4)  // 第 3 轮起才降尺寸
//!   仍不达标就按最后一次结果发（不无限重试；极端文字截图兜底）
//! 即尺寸序列为 1568 → 1568 → 1176(→896) → 896，质量序列 80 → 65 → 55 → 55。
//!
//! ── 与原实现的两处实现差异（语义等价或更保守，报告里已写明）─────
//! 1. **编码器不同**：原实现用 sharp + mozjpeg（trellis 量化 + 4:2:0 色度抽样），
//!    本模块用 image crate 自带的基线 JPEG 编码器（4:4:4，无色度抽样）。同质量下
//!    本实现产物偏大，因此更容易走到后续降质/降尺寸轮 —— 但每轮都以**实测输出
//!    大小**判定，达标条件与最终产物格式（`data:image/jpeg;base64,`）不变。
//! 2. **缩放核**：sharp 默认 lanczos3，这里显式用 `FilterType::Lanczos3` 对齐。
//!    `fit:'inside'` + `withoutEnlargement:true` 由「两边都不超上限才跳过缩放」
//!    等价实现（image crate 的 resize 没有不放大开关，得自己挡）。
//! 3. **透明像素**：原实现 `.flatten({background:'#ffffff'})` 把 alpha 合成到白底；
//!    本模块同样合成到白底（16 位/浮点位深经 to_rgba8 归一后再合成）。
//! 4. **不支持的输入**：SVG（原实现靠 sharp 能解码）在 image crate 里解不开 ——
//!    直接原样返回，不阻断请求（属于降级，不是崩溃）。
//!
//! ── panic = abort 的硬约束 ─────────────────────────────────
//! release 是 panic=abort，本模块在对话链路上，因此**全程不 unwrap / expect /
//! panic**：格式不符、base64 解不开、图片解不开、编码失败、尺寸非法 —— 一律
//! 原样返回输入字符串。宁可超重发出去（上游报错可见），也不让整个应用被一张
//! 坏图带走。纯内存操作，无网络、无磁盘 IO。
//!
//! ── 调用方式（关于「同步阻塞」）─────────────────────────────
//! `compress_if_needed` 是**同步函数**：纯 CPU 活、有界（最多 4 次 ≤1568px 的
//! 缩放+编码），与 JS 原实现的 async 签名不同 —— 但注意 sharp 的活是跑在
//! Node 的线程池里，并不占事件循环；Rust 侧若在 tokio worker 上直接调，会占用
//! 该 worker 若干百毫秒。调用点（catpaw 的消息归一化）如需完全避免阻塞，用
//! `tokio::task::spawn_blocking` 包一层即可；本模块不做异步化，免得给纯 CPU
//! 函数套上不必要的 Future 传染。

use std::io::Cursor;

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, RgbImage};

/// 触发阈值：**Base64 文本长度**（解码前，含可能存在的 \r\n）达到就值得压。
const COMPRESS_THRESHOLD_BASE64: usize = 60 * 1024;
/// 达标目标：压缩产物的**二进制长度**（原实现的 `output.length` 是 Buffer 字节数）。
const TARGET_OUTPUT_BYTES: usize = 120 * 1024;
/// 最长边上限（`fit: inside`，等比缩放，不放大）
const MAX_DIMENSION: u32 = 1568;
/// 降尺寸下限（再降也不会低于它）
const MIN_DIMENSION: u32 = 896;
/// 首轮 JPEG 质量
const JPEG_QUALITY: u8 = 80;
/// 质量下限
const MIN_QUALITY: u8 = 55;
/// 最多编码次数（尺寸/质量降到底仍不达标就按最后一次结果发）
const MAX_ATTEMPTS: u32 = 4;

/// data URL 前缀（原实现的正则 `^data:image\/`，大小写敏感）
const DATA_URL_PREFIX: &str = "data:image/";

/// base64 引擎：标准字母表，但**解码放宽到贴近 Node 的 `Buffer.from(x, 'base64')`**。
///
/// 原实现用 `Buffer.from(base64, 'base64')`，它对填充缺失与非零尾随位都宽容
/// （填充不足时照样解出整数个字节）；base64 crate 的 `STANDARD` 则要求规范填充
/// 且尾随位为零。这里对齐 Node 的宽容度（`Indifferent` 允许少填充、
/// `allow_trailing_bits` 允许非零尾随位）—— 松一点只会让我们多压几张图，
/// 而收紧会把本该压的图原样放行。编码方向仍用规范填充（`with_encode_padding`
/// 缺省即 true），因为产物要交给上游客户端解析。
const DATA_URL_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// 压缩内联图片：超阈值的 `data:image/*;base64,` 图片压成 JPEG，其余原样返回。
///
/// 返回 `String` 而不是 `Cow`：压缩成功时必然要新建字符串，失败时按原实现
/// 语义返回「原字符串内容」，调用方直接替换字段值即可，不需要再判生命周期。
///
/// 这里不做「是否为图片块」的判断（原实现的 `compressContentBlocks` 那层：
/// 只认 `type == image_url`、只处理 `data:image/` 开头、保留 `detail` 字段并
/// 复制其它字段）—— 那属于消息块的归一化，落在同目录的 `messages.rs`，本函数
/// 只负责「一个 data URL → 一个（可能压缩过的）data URL」。
///
/// 死代码抑制不在这里逐个函数标：本目录的调用方连线在后续波次（T-d3 / T-d4），
/// `mod.rs` 已做模块级 `#![allow(dead_code)]`，接线时统一摘除。
pub fn compress_if_needed(data_url: &str) -> String {
    // 1. 形态判定：不匹配原实现正则的输入一律不碰（http 图片、裸 base64、
    //    大小写不同的前缀、含非法字符的 base64 都走这条早退）。
    let Some(base64_body) = image_data_url_body(data_url) else {
        return data_url.to_string();
    };
    // 2. 阈值判定在**解码前**、按 base64 文本长度算（含换行的原始长度）。
    if base64_body.len() < COMPRESS_THRESHOLD_BASE64 {
        return data_url.to_string();
    }

    // 3. 解码。原实现只允许 `[A-Za-z0-9+/=\r\n]`，其中换行在解码时被忽略，
    //    这里先把 \r\n 去掉再交给 base64 引擎（容错度见 DATA_URL_BASE64）。
    let compact: String = base64_body
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect();
    let Ok(input) = DATA_URL_BASE64.decode(compact.as_bytes()) else {
        return data_url.to_string();
    };
    let Some(decoded) = decode_image(&input) else {
        return data_url.to_string();
    };

    // 4. 逐级降质 / 降尺寸（退出条件见模块头注释）。
    let mut dimension = MAX_DIMENSION;
    let mut quality = JPEG_QUALITY;
    let mut output: Option<Vec<u8>> = None;
    for attempt in 0..MAX_ATTEMPTS {
        let Some(encoded) = encode_attempt(&decoded, dimension, quality) else {
            return data_url.to_string();
        };
        let reached_target = encoded.len() <= TARGET_OUTPUT_BYTES;
        output = Some(encoded);
        if reached_target {
            break;
        }
        quality = quality.saturating_sub(15).max(MIN_QUALITY);
        if attempt >= 1 {
            // 尺寸序列里的 1568/1176/896 都是 4 的倍数，整除与 JS 的
            // `Math.round(dimension * 0.75)` 逐位相等（不会出现半值）。
            dimension = (dimension * 3 / 4).max(MIN_DIMENSION);
        }
    }

    // 5. 压了反而更大（或无产物）就保留原图：原实现的
    //    `if (!output || output.length >= input.length) return url;`
    let Some(encoded) = output else {
        return data_url.to_string();
    };
    if encoded.len() >= input.len() {
        return data_url.to_string();
    }
    format!(
        "data:image/jpeg;base64,{}",
        DATA_URL_BASE64.encode(&encoded)
    )
}

/// 取出 `data:image/<subtype>;base64,<body>` 里的 base64 主体。
///
/// 逐条对齐原实现的正则 `^data:image\/([a-zA-Z0-9.+-]+);base64,([A-Za-z0-9+/=\r\n]+)$`：
/// 前缀大小写敏感、subtype 至少 1 个字符且只含 `A-Za-z0-9.+-`、主体至少 1 个
/// 字符且只含 `A-Za-z0-9+/=` 与换行、且必须整串匹配（尾部多余字符即不匹配）。
fn image_data_url_body(data_url: &str) -> Option<&str> {
    let rest = data_url.strip_prefix(DATA_URL_PREFIX)?;
    // subtype 里不能出现 ';'，所以第一个 ';' 就是分隔点（与正则的左到右匹配一致）
    let separator = rest.find(';')?;
    let subtype = &rest[..separator];
    if subtype.is_empty()
        || !subtype
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '+' | '-'))
    {
        return None;
    }
    let body = rest[separator..].strip_prefix(";base64,")?;
    if body.is_empty()
        || !body
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '\r' | '\n'))
    {
        return None;
    }
    Some(body)
}

/// 解码图片字节（格式靠魔数猜：PNG / JPEG / WebP 由 Cargo.toml 的 image 特性开启）。
///
/// 用默认解码上限（image crate 的 `Limits::default()`，max_alloc 512MiB）：
/// 超大图/解压炸弹会返回 Err 而不是吃光内存 —— 失败即原样返回，不阻断请求。
fn decode_image(bytes: &[u8]) -> Option<DynamicImage> {
    image::load_from_memory(bytes).ok()
}

/// 一次编码尝试：按 `dimension` 等比缩到上限内（不放大）→ alpha 合成白底 → JPEG。
///
/// 顺序与链接方式对齐原实现的 `sharp(input).resize(...).flatten(...).jpeg(...)`。
/// 「不放大」由 `needs_shrink` 自己挡（image crate 的 resize 没有这个开关）；
/// 不需要缩时就地借用原图，省掉一次整图克隆（大图上是 MB 级的内存拷贝）。
fn encode_attempt(image: &DynamicImage, dimension: u32, quality: u8) -> Option<Vec<u8>> {
    let shrunk;
    let source = if image.width() <= dimension && image.height() <= dimension {
        image
    } else {
        // `fit: inside` 语义：宽高给同一个上限 + 等比 → 最长边 ≤ dimension
        shrunk = image.resize(dimension, dimension, FilterType::Lanczos3);
        &shrunk
    };
    let rgb = flatten_to_rgb8(source)?;
    let (width, height) = rgb.dimensions();
    if width == 0 || height == 0 {
        // JPEG 编码器内部像素取样会用 `width - 1` 兜底，0 尺寸会踩到它的边界假设，
        // 这里提前挡住（解不开的图原样返回，不让请求崩）。
        return None;
    }
    let mut output = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(Cursor::new(&mut output), quality);
    encoder.encode_image(&rgb).ok()?;
    Some(output)
}

/// 拍平成不含 alpha 的 8 位 RGB：有 alpha 的按白底合成，没有的直接转 RGB8。
///
/// 等价于原实现的 `.flatten({ background: '#ffffff' })`。合成公式是直通道
/// alpha 的 over 运算取整：`(c * a + 255 * (255 - a) + 127) / 255`（a=255 时
/// 恒等、a=0 时为白）—— 与 libvips 的内部取整可能差 1 个色阶，肉眼不可见。
fn flatten_to_rgb8(image: &DynamicImage) -> Option<RgbImage> {
    if !image.color().has_alpha() {
        // 灰度/RGB 直接转（image crate 自己处理位深与通道扩展）
        return Some(image.to_rgb8());
    }
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let pixels = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(3);
    let mut data = Vec::new();
    if data.try_reserve_exact(pixels).is_err() {
        return None;
    }
    for pixel in rgba.pixels() {
        let channels = pixel.0;
        let alpha = u32::from(channels[3]);
        let opaque = 255 - alpha;
        for channel in &channels[..3] {
            let blended = (u32::from(*channel) * alpha + 255 * opaque + 127) / 255;
            data.push(blended as u8);
        }
    }
    RgbImage::from_raw(width, height, data)
}
