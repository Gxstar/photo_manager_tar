//! CN-CLIP 推理（docs §38 第二批）。
//!
//! - 运行时：`resources/onnxruntime.dll`，`load-dynamic` 加载，不依赖 pyke CDN；
//! - 模型：`models/cnclip-vit-b16/model_quantized.onnx` + `tokenizer.json`；
//! - 输出：512 维、已 L2 归一化（本机实测 norm=1.0000），点积即余弦相似度。
//!
//! 只在本机推理，绝不上传图片、路径、向量（§17.13 硬边界）。

use image::{DynamicImage, imageops::FilterType};
use ort::environment::Environment;
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use tokenizers::Tokenizer;

pub const CLIP_SIZE: u32 = 224;
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

/// DLL 路径：Tauri setup 时按 resource_dir / 开发目录解析后写入。
static DLL: OnceLock<PathBuf> = OnceLock::new();
/// ORT 环境只能初始化一次；模型加载失败可重试，DLL 不重复加载。
static ENV: OnceLock<Option<Environment>> = OnceLock::new();
static CLIP: LazyLock<Mutex<Option<Clip>>> = LazyLock::new(|| Mutex::new(None));

pub fn set_dll(path: PathBuf) {
    let _ = DLL.set(path);
}

fn dll_path() -> PathBuf {
    if let Some(p) = DLL.get() {
        return p.clone();
    }
    // 开发期兜底：crate 根 / resources
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/onnxruntime.dll")
}

pub(crate) fn env() -> Option<&'static Environment> {
    ENV.get_or_init(|| {
        let builder = match ort::init_from(dll_path()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[smart] ONNX Runtime 加载失败（{}）: {e}",
                    dll_path().display()
                );
                return None;
            }
        };
        match builder.build() {
            Ok(env) => Some(env),
            Err(e) => {
                eprintln!("[smart] ORT 环境初始化失败: {e}");
                None
            }
        }
    })
    .as_ref()
}

pub struct Clip {
    session: Session,
    tokenizer: Tokenizer,
}

impl Clip {
    fn load() -> Result<Self, String> {
        let env = env().ok_or_else(|| {
            format!(
                "ONNX Runtime 未能加载，请确认 {} 存在",
                dll_path().display()
            )
        })?;
        let dir = crate::smart::models_dir().join("cnclip-vit-b16");
        let model = dir.join("model_quantized.onnx");
        let tok = dir.join("tokenizer.json");
        if !model.is_file() || !tok.is_file() {
            return Err("语义搜图模型未下载".into());
        }
        let tokenizer =
            Tokenizer::from_file(&tok).map_err(|e| format!("tokenizer 加载失败: {e}"))?;
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 4).clamp(2, 8))
            .unwrap_or(4);
        let session = Session::builder(env)
            .map_err(|e| format!("创建 ORT 会话失败: {e}"))?
            .with_intra_threads(threads)
            .map_err(|e| format!("设置线程数失败: {e}"))?
            .commit_from_file(&model)
            .map_err(|e| format!("模型加载失败: {e}"))?;
        Ok(Self { session, tokenizer })
    }

    /// 图像 → 512 维向量。索引时复用 256px 缩略图，避免重复读原图。
    pub fn encode_image(&mut self, img: &DynamicImage) -> Result<Vec<f32>, String> {
        let pixels = preprocess(img);
        // 单文件双塔：必须同时喂文本。用 [CLS][SEP] 占位，文本塔成本很小。
        let ids = Tensor::from_array((vec![1i64, 2], vec![101i64, 102]))
            .map_err(|e| format!("构造输入失败: {e}"))?;
        let mask = Tensor::from_array((vec![1i64, 2], vec![1i64, 1]))
            .map_err(|e| format!("构造输入失败: {e}"))?;
        let pixel = Tensor::from_array((vec![1i64, 3, CLIP_SIZE as i64, CLIP_SIZE as i64], pixels))
            .map_err(|e| format!("构造输入失败: {e}"))?;
        let out = self
            .session
            .run(ort::inputs![
                "pixel_values" => pixel,
                "input_ids" => ids,
                "attention_mask" => mask
            ])
            .map_err(|e| format!("图像推理失败: {e}"))?;
        let (_, data) = out["image_embeds"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("读取图像向量失败: {e}"))?;
        Ok(data.to_vec())
    }

    /// 查询文本 → 512 维向量。图像输入用全 0 占位（模型要求双输入）。
    pub fn encode_text(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| format!("分词失败: {e}"))?;
        let mut ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
        let mut mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
        if ids.is_empty() {
            return Err("查询为空".into());
        }
        // tokenizer.json 若未配置 post_processor，手动补 [CLS] / [SEP]
        if ids.first() != Some(&101) {
            ids.insert(0, 101);
            mask.insert(0, 1);
        }
        if ids.last() != Some(&102) {
            ids.push(102);
            mask.push(1);
        }
        if ids.len() > 77 {
            return Err("查询过长，请精简后再试".into());
        }
        let seq = ids.len() as i64;
        let pixel = Tensor::from_array((
            vec![1i64, 3, CLIP_SIZE as i64, CLIP_SIZE as i64],
            vec![0f32; 3 * (CLIP_SIZE * CLIP_SIZE) as usize],
        ))
        .map_err(|e| format!("构造输入失败: {e}"))?;
        let ids = Tensor::from_array((vec![1i64, seq], ids))
            .map_err(|e| format!("构造输入失败: {e}"))?;
        let mask = Tensor::from_array((vec![1i64, seq], mask))
            .map_err(|e| format!("构造输入失败: {e}"))?;
        let out = self
            .session
            .run(ort::inputs![
                "pixel_values" => pixel,
                "input_ids" => ids,
                "attention_mask" => mask
            ])
            .map_err(|e| format!("文本推理失败: {e}"))?;
        let (_, data) = out["text_embeds"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("读取文本向量失败: {e}"))?;
        Ok(data.to_vec())
    }
}

/// 线程安全入口：模型按需加载；加载失败不缓存，下次可重试（例如刚下载完）。
pub fn with_clip<T>(f: impl FnOnce(&mut Clip) -> Result<T, String>) -> Result<T, String> {
    let mut guard = CLIP.lock().map_err(|_| "推理被占用".to_string())?;
    if guard.is_none() {
        *guard = Some(Clip::load()?);
    }
    f(guard.as_mut().unwrap())
}

/// 把 256px 缩略图解码成 DynamicImage；失败返回原因，索引侧按失败计数。
pub fn decode_thumb(bytes: &[u8]) -> Result<DynamicImage, String> {
    image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|e| format!("缩略图解码失败: {e}"))
}

fn preprocess(img: &DynamicImage) -> Vec<f32> {
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = CLIP_SIZE as f32 / w.min(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(CLIP_SIZE);
    let nh = ((h as f32 * scale).round() as u32).max(CLIP_SIZE);
    let resized = img.resize_exact(nw, nh, FilterType::Triangle);
    let x = (nw - CLIP_SIZE) / 2;
    let y = (nh - CLIP_SIZE) / 2;
    let rgb = resized
        .crop_imm(x, y, CLIP_SIZE, CLIP_SIZE)
        .to_rgb8();
    let plane = (CLIP_SIZE * CLIP_SIZE) as usize;
    let mut out = vec![0f32; 3 * plane];
    for (i, px) in rgb.pixels().enumerate() {
        for c in 0..3 {
            out[c * plane + i] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    out
}

/// 主体分类的固定标签集（零样本）。只保留**主体 / 明确场景**，
/// 去掉「海边 / 街头 / 树林 / 夜景」这类泛化词——实测它们会把整库吸走，
/// 分类卡数量看着大，点进去大量不相关（2026-11-12 实测分布）。
pub const THING_LABELS: &[&str] = &[
    "人物", "猫", "狗", "鸟", "花", "美食", "汽车", "婴儿", "建筑", "雪山", "日落",
];

/// 零样本的提示模板。**实测极其关键**：用「美食」单词打分时，草原、古镇街景、
/// 云、丹霞地貌都被判成美食（9 张人工确认样本里 6 张误判）；换成
/// 「一张{label}的照片」后，这 6 张全部落到更合理的标签（人物 / 建筑 / 日落）上，
/// 美食只剩真正的两张食物。CLIP 的文本塔见过的大多是自然语句，单词远离训练分布。
fn label_prompt(label: &str) -> String {
    format!("一张{label}的照片")
}

/// 分类门槛（2026-11-13 用 9 张人工确认样本标定）：
/// - `cos` 太低说明这张图和任何标签都不像；
/// - `conf` 是 cos×100 做 softmax 后 top1 的概率，**真正把「勉强最像」挡在外面的是它**。
///   实测真食物 conf 0.94 / 0.996，误判的风景片 0.27–0.72。
pub const THING_MIN_COS: f32 = 0.34;
pub const THING_MIN_CONF: f32 = 0.75;

static THINGS: LazyLock<Mutex<Option<Vec<(&'static str, Vec<f32>)>>>> =
    LazyLock::new(|| Mutex::new(None));

/// 标签文本向量：首次编码 11 条约 1 秒，之后常驻内存。
pub fn thing_vectors() -> Result<Vec<(&'static str, Vec<f32>)>, String> {
    let mut guard = THINGS.lock().map_err(|_| "标签缓存被占用".to_string())?;
    if let Some(list) = guard.as_ref() {
        return Ok(list.clone());
    }
    let mut list: Vec<(&'static str, Vec<f32>)> = Vec::with_capacity(THING_LABELS.len());
    for label in THING_LABELS {
        let vec = with_clip(|c| c.encode_text(&label_prompt(label)))?;
        list.push((*label, vec));
    }
    *guard = Some(list.clone());
    Ok(list)
}

/// 单张图 → 标签得分（降序）。点积即余弦；int8 模型分数偏低，阈值由调用方定。
pub fn score_things(v: &[f32]) -> Result<Vec<(&'static str, f32)>, String> {
    let labels = thing_vectors()?;
    let mut scored: Vec<(&'static str, f32)> = labels
        .iter()
        .map(|(label, tv)| {
            (
                *label,
                v.iter().zip(tv.iter()).map(|(a, b)| a * b).sum::<f32>(),
            )
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored)
}

/// top1 的零样本置信度：`softmax(cos × 100)` 的最大概率。
/// CLIP 的 logit 尺度约定就是 ×100，乘完后「真正的匹配」会拉开到 0.9+，
/// 「都不像」则摊平在 1/11 ≈ 0.09–0.7 之间。
pub fn thing_confidence(scores: &[(&str, f32)]) -> f32 {
    let Some((_, best)) = scores.first() else {
        return 0.0;
    };
    let sum: f32 = scores.iter().map(|(_, s)| ((s - best) * 100.0).exp()).sum();
    if sum <= f32::EPSILON {
        return 0.0;
    }
    1.0 / sum
}

/// 判定一张图该归到哪个标签：都不过门槛就返回 None（宁可不归类，也不要错归）。
pub fn assign_thing(v: &[f32]) -> Option<(&'static str, f32, f32)> {
    let scored = score_things(v).ok()?;
    let conf = thing_confidence(&scored);
    let (label, cos) = *scored.first()?;
    if cos >= THING_MIN_COS && conf >= THING_MIN_CONF {
        Some((label, cos, conf))
    } else {
        None
    }
}

/// 给测试 / 诊断用：模型文件是否齐（不加载）。
pub fn model_present() -> bool {
    let dir = crate::smart::models_dir().join("cnclip-vit-b16");
    [dir.join("model_quantized.onnx"), dir.join("tokenizer.json")]
        .iter()
        .all(|p| p.is_file())
}

#[allow(dead_code)]
pub fn assert_paths(root: &Path) {
    let _ = root;
}
