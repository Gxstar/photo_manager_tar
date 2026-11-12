//! 人脸检测与身份向量（docs §38 人物批）。
//!
//! - YuNet 2023mar：输入 640×640 RGB（0–255，swapRB 后即 RGB），输出 stride 8/16/32
//!   的 cls/obj/bbox/kps，本文件负责解码 + NMS；
//! - SFace 2021dec：输入 112×112 RGB（0–255，BGR 通道序），输出 128 维身份向量；
//! - 编码前用 YuNet 的 5 点做相似变换对齐到 ArcFace 模板（关键点无效时回退中心裁剪）。
//!
//! 人脸框与向量只写本机 `smart.db`，不写回照片、不上传（§17.7 硬边界）。

use image::{DynamicImage, imageops::FilterType};
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

const DET_SIZE: u32 = 640;
const DET_THRESHOLD: f32 = 0.7;
const NMS_IOU: f32 = 0.3;
const FACE_SIZE: u32 = 112;

/// 扫描用图的长边：实测本机媒体库人脸框中位仅 21px（256px 缩略图坐标系），
/// 放大到 112 喂 SFace 等于把噪声放大 5 倍，是「同一个人聚不到一起」的根因。
///
/// 取 640 而不是更大：**实测 1024 反而更差**——同一对照片（A2/B1，同一人）
/// 在 640 下余弦 0.426、1024 下只有 0.314（低于阈值 0.36），不同人则仍是 0.16–0.21。
/// 分辨率不是越高越好，SFace 的训练尺度就在中等分辨率人脸上。
pub const SCAN_MAX_SIDE: u32 = 640;

/// 解码一张照片用于人脸扫描：等比缩到长边 `max_side`。
/// 用 `thumbnail`（整数快速采样）而不是 Triangle 重采样——24MP 下前者快一个量级。
pub fn load_for_scan(path: &Path, max_side: u32) -> Result<DynamicImage, String> {
    let img = image::open(path).map_err(|e| format!("解码失败: {e}"))?;
    Ok(img.thumbnail(max_side, max_side))
}

#[derive(Debug, Clone)]
pub struct DetectedFace {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub score: f32,
    /// 5 点关键点（左眼、右眼、鼻尖、左嘴角、右嘴角），原图坐标
    pub kps: [[f32; 2]; 5],
}

/// ArcFace / SFace 的标准 112×112 五点模板
const TEMPLATE: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

pub struct FaceEngine {
    yunet: Session,
    sface: Session,
}

static ENGINE: LazyLock<Mutex<Option<FaceEngine>>> = LazyLock::new(|| Mutex::new(None));

pub fn with_faces<T>(f: impl FnOnce(&mut FaceEngine) -> Result<T, String>) -> Result<T, String> {
    let mut guard = ENGINE
        .lock()
        .map_err(|_| "人脸引擎被占用".to_string())?;
    if guard.is_none() {
        *guard = Some(FaceEngine::load()?);
    }
    f(guard.as_mut().unwrap())
}

pub fn models_present() -> bool {
    let dir = crate::smart::models_dir().join("faces");
    [dir.join("yunet.onnx"), dir.join("sface.onnx")]
        .iter()
        .all(|p| p.is_file())
}

impl FaceEngine {
    fn load() -> Result<Self, String> {
        let env = crate::smart::clip::env().ok_or_else(|| {
            "ONNX Runtime 未能加载，请确认 resources/onnxruntime.dll 存在".to_string()
        })?;
        let dir = crate::smart::models_dir().join("faces");
        let yunet_path = dir.join("yunet.onnx");
        let sface_path = dir.join("sface.onnx");
        if !yunet_path.is_file() || !sface_path.is_file() {
            return Err("人脸模型未下载".into());
        }
        let yunet = Session::builder(env)
            .map_err(|e| format!("创建 YuNet 会话失败: {e}"))?
            .with_intra_threads(2)
            .map_err(|e| format!("设置线程数失败: {e}"))?
            .commit_from_file(&yunet_path)
            .map_err(|e| format!("YuNet 加载失败: {e}"))?;
        let sface = Session::builder(env)
            .map_err(|e| format!("创建 SFace 会话失败: {e}"))?
            .with_intra_threads(2)
            .map_err(|e| format!("设置线程数失败: {e}"))?
            .commit_from_file(&sface_path)
            .map_err(|e| format!("SFace 加载失败: {e}"))?;
        Ok(Self { yunet, sface })
    }

    /// 检测人脸（坐标已映射回原图）。
    pub fn detect(&mut self, img: &DynamicImage) -> Result<Vec<DetectedFace>, String> {
        let (w, h) = (img.width().max(1), img.height().max(1));
        // 24MP 直接 Triangle 缩到 640 要 2–3 秒；thumbnail 是整数快速采样，
        // 先等比缩到 ≤640，再拉伸到 640×640（与直接拉伸等价，坐标映射不变）。
        let thumb = img.thumbnail(DET_SIZE, DET_SIZE);
        let resized = thumb
            .resize_exact(DET_SIZE, DET_SIZE, FilterType::Triangle)
            .to_rgb8();
        let plane = (DET_SIZE * DET_SIZE) as usize;
        let mut data = vec![0f32; 3 * plane];
        for (i, px) in resized.pixels().enumerate() {
            // OpenCV 侧是 swapRB=true + scale=1.0，等价于 RGB 原始 0–255
            data[i] = px[0] as f32;
            data[plane + i] = px[1] as f32;
            data[2 * plane + i] = px[2] as f32;
        }
        let tensor = Tensor::from_array((
            vec![1i64, 3, DET_SIZE as i64, DET_SIZE as i64],
            data,
        ))
        .map_err(|e| format!("构造检测输入失败: {e}"))?;
        let out = self
            .yunet
            .run(ort::inputs!["input" => tensor])
            .map_err(|e| format!("人脸检测失败: {e}"))?;

        let sx = w as f32 / DET_SIZE as f32;
        let sy = h as f32 / DET_SIZE as f32;
        let mut faces = Vec::new();
        for stride in [8u32, 16, 32] {
            let grid = DET_SIZE / stride;
            let cls_name = format!("cls_{stride}");
            let obj_name = format!("obj_{stride}");
            let bbox_name = format!("bbox_{stride}");
            let kps_name = format!("kps_{stride}");
            let (_, cls) = out[cls_name.as_str()]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("读取 cls 失败: {e}"))?;
            let (_, obj) = out[obj_name.as_str()]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("读取 obj 失败: {e}"))?;
            let (_, bbox) = out[bbox_name.as_str()]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("读取 bbox 失败: {e}"))?;
            let (_, kps) = out[kps_name.as_str()]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("读取 kps 失败: {e}"))?;
            for i in 0..grid {
                for j in 0..grid {
                    let idx = (i * grid + j) as usize;
                    let score = (cls[idx] * obj[idx]).max(0.0).sqrt();
                    if score < DET_THRESHOLD {
                        continue;
                    }
                    let cx = (j as f32 + bbox[idx * 4]) * stride as f32;
                    let cy = (i as f32 + bbox[idx * 4 + 1]) * stride as f32;
                    let bw = bbox[idx * 4 + 2].exp() * stride as f32;
                    let bh = bbox[idx * 4 + 3].exp() * stride as f32;
                    let x1 = ((cx - bw / 2.0) * sx).max(0.0);
                    let y1 = ((cy - bh / 2.0) * sy).max(0.0);
                    let x2 = ((cx + bw / 2.0) * sx).min(w as f32);
                    let y2 = ((cy + bh / 2.0) * sy).min(h as f32);
                    if x2 - x1 < 8.0 || y2 - y1 < 8.0 {
                        continue;
                    }
                    let mut pts = [[0f32; 2]; 5];
                    for k in 0..5 {
                        pts[k][0] = (j as f32 + kps[idx * 10 + 2 * k]) * stride as f32 * sx;
                        pts[k][1] = (i as f32 + kps[idx * 10 + 2 * k + 1]) * stride as f32 * sy;
                    }
                    faces.push(DetectedFace {
                        x1,
                        y1,
                        x2,
                        y2,
                        score,
                        kps: pts,
                    });
                }
            }
        }
        Ok(nms(faces, NMS_IOU))
    }

    /// 人脸 → (128 维身份向量, 112×112 对齐人脸图)。
    /// 向量已 L2 归一化（点积即余弦）；对齐图用作簇 / 人物册封面，
    /// 避免整张照片当封面时看不到脸。
    /// 用 5 点关键点做相似变换对齐到 112×112 标准模板；关键点无效时回退中心裁剪。
    pub fn embed(
        &mut self,
        img: &DynamicImage,
        f: &DetectedFace,
    ) -> Result<(Vec<f32>, image::RgbImage), String> {
        let src = img.to_rgb8();
        let crop = align(&src, &f.kps).unwrap_or_else(|| center_crop(&src, f));
        let plane = (FACE_SIZE * FACE_SIZE) as usize;
        let mut data = vec![0f32; 3 * plane];
        for (i, px) in crop.pixels().enumerate() {
            // 试 0–255（部分 SFace ONNX 导出把 /255 放在图内）
            data[i] = px[2] as f32;
            data[plane + i] = px[1] as f32;
            data[2 * plane + i] = px[0] as f32;
        }
        let tensor = Tensor::from_array((
            vec![1i64, 3, FACE_SIZE as i64, FACE_SIZE as i64],
            data,
        ))
        .map_err(|e| format!("构造识别输入失败: {e}"))?;
        let out = self
            .sface
            .run(ort::inputs!["data" => tensor])
            .map_err(|e| format!("人脸特征提取失败: {e}"))?;
        let (_, vec) = out["fc1"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("读取人脸向量失败: {e}"))?;
        let mut v = vec.to_vec();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-6 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok((v, crop))
    }
}

/// 把对齐后的 112×112 人脸写成 JPEG 封面（质量 90 → 约 6–10KB）。
pub fn save_face_cover(dir: &Path, name: &str, crop: &image::RgbImage) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{name}.jpg"));
    let img = DynamicImage::ImageRgb8(crop.clone());
    img.save_with_format(&path, image::ImageFormat::Jpeg).ok()?;
    Some(path.to_string_lossy().to_string())
}

/// 5 点相似变换对齐；关键点无效返回 None。
fn align(src: &image::RgbImage, kps: &[[f32; 2]; 5]) -> Option<image::RgbImage> {
    if kps.iter().all(|p| p[0] == 0.0 && p[1] == 0.0) {
        return None;
    }
    let mut ata = [[0f64; 4]; 4];
    let mut atb = [0f64; 4];
    for k in 0..5 {
        let x = kps[k][0] as f64;
        let y = kps[k][1] as f64;
        let u = TEMPLATE[k][0] as f64;
        let v = TEMPLATE[k][1] as f64;
        for row in [[x, -y, 1.0, 0.0, u], [y, x, 0.0, 1.0, v]] {
            for i in 0..4 {
                for j in 0..4 {
                    ata[i][j] += row[i] * row[j];
                }
                atb[i] += row[i] * row[4];
            }
        }
    }
    let sol = solve4(ata, atb)?;
    let (a, b, tx, ty) = (sol[0], sol[1], sol[2], sol[3]);
    let det = a * a + b * b;
    if det < 1e-12 {
        return None;
    }
    let (sw, sh) = (src.width() as f64, src.height() as f64);
    let mut out = image::RgbImage::new(FACE_SIZE, FACE_SIZE);
    for v in 0..FACE_SIZE {
        for u in 0..FACE_SIZE {
            let du = u as f64 - tx;
            let dv = v as f64 - ty;
            let x = (a * du + b * dv) / det;
            let y = (-b * du + a * dv) / det;
            if x < 0.0 || y < 0.0 || x >= sw - 1.0 || y >= sh - 1.0 {
                continue;
            }
            let x0 = x.floor() as u32;
            let y0 = y.floor() as u32;
            let fx = (x - x0 as f64) as f32;
            let fy = (y - y0 as f64) as f32;
            let p00 = src.get_pixel(x0, y0);
            let p10 = src.get_pixel(x0 + 1, y0);
            let p01 = src.get_pixel(x0, y0 + 1);
            let p11 = src.get_pixel(x0 + 1, y0 + 1);
            let mut px = [0u8; 3];
            for c in 0..3 {
                let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
                let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
                px[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
            }
            out.put_pixel(u, v, image::Rgb(px));
        }
    }
    Some(out)
}

/// 4×4 线性方程组（高斯消元 + 部分主元）
fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for i in 0..4 {
        let mut pivot = i;
        for r in (i + 1)..4 {
            if a[r][i].abs() > a[pivot][i].abs() {
                pivot = r;
            }
        }
        if a[pivot][i].abs() < 1e-12 {
            return None;
        }
        a.swap(i, pivot);
        b.swap(i, pivot);
        for r in (i + 1)..4 {
            let f = a[r][i] / a[i][i];
            for c in i..4 {
                a[r][c] -= f * a[i][c];
            }
            b[r] -= f * b[i];
        }
    }
    let mut x = [0f64; 4];
    for i in (0..4).rev() {
        let mut s = b[i];
        for c in (i + 1)..4 {
            s -= a[i][c] * x[c];
        }
        x[i] = s / a[i][i];
    }
    Some(x)
}

/// 回退：关键点缺失时按框中心正方形裁剪（外扩 20%）。
fn center_crop(src: &image::RgbImage, f: &DetectedFace) -> image::RgbImage {
    let cx = (f.x1 + f.x2) / 2.0;
    let cy = (f.y1 + f.y2) / 2.0;
    let side = ((f.x2 - f.x1).max(f.y2 - f.y1) * 1.2).max(8.0);
    let iw = src.width() as f32;
    let ih = src.height() as f32;
    let x1 = (cx - side / 2.0).clamp(0.0, iw - 1.0);
    let y1 = (cy - side / 2.0).clamp(0.0, ih - 1.0);
    let x2 = (cx + side / 2.0).clamp(x1 + 1.0, iw);
    let y2 = (cy + side / 2.0).clamp(y1 + 1.0, ih);
    let sub = image::imageops::crop_imm(
        src,
        x1 as u32,
        y1 as u32,
        (x2 - x1).max(1.0) as u32,
        (y2 - y1).max(1.0) as u32,
    )
    .to_image();
    image::imageops::resize(&sub, FACE_SIZE, FACE_SIZE, FilterType::Triangle)
}

fn iou(a: &DetectedFace, b: &DetectedFace) -> f32 {
    let x1 = a.x1.max(b.x1);
    let y1 = a.y1.max(b.y1);
    let x2 = a.x2.min(b.x2);
    let y2 = a.y2.min(b.y2);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area_a = (a.x2 - a.x1) * (a.y2 - a.y1);
    let area_b = (b.x2 - b.x1) * (b.y2 - b.y1);
    if area_a + area_b - inter <= 0.0 {
        return 0.0;
    }
    inter / (area_a + area_b - inter)
}

fn nms(mut faces: Vec<DetectedFace>, threshold: f32) -> Vec<DetectedFace> {
    faces.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<DetectedFace> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| iou(k, &f) <= threshold) {
            keep.push(f);
        }
    }
    keep
}
