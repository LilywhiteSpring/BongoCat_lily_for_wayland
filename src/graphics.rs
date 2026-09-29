//! 图片加载、预处理与画面合成。
//!
//! 所有 PNG 素材会在启动时解码、预乘 alpha、缩放到目标窗口尺寸，
//! 并裁剪透明边缘。实际渲染时只需要按层顺序混合到 `wl_shm` 缓冲区。

use crate::config::AssetsConfig;
use std::{fs::File, io::BufReader, path::Path};

/// 一帧画面需要展示的手部动作和键盘高亮状态。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VisualState {
    pub left_pose: Option<usize>,
    pub right_pose: Option<usize>,
    pub highlight: Option<usize>,
}

/// 一张已解码并裁剪好的透明图层。
///
/// 像素格式为预乘 alpha 的 BGRA，顺序与 `wl_shm` 的 ARGB8888 写入目标一致。
#[derive(Debug)]
struct Layer {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>, // premultiplied BGRA
}

/// 一次加载完成后的完整渲染场景。
///
/// 启动后保持不变，渲染时根据 [`VisualState`] 选择图层组合。
pub struct Scene {
    width: u32,
    height: u32,
    background: Layer,
    keyboard: Vec<Layer>,
    left_up: Layer,
    left_down: Vec<Layer>,
    right_up: Layer,
    right_down: Vec<Layer>,
}

impl Scene {
    /// 从素材配置加载所有图层，并缩放到指定窗口尺寸。
    pub fn load(
        config: &AssetsConfig,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if width == 0 || height == 0 {
            return Err("surface size must be non-zero".into());
        }
        let load = |relative: &Path| load_layer(&config.root.join(relative), width, height);
        Ok(Self {
            width,
            height,
            background: load(&config.background)?,
            keyboard: config
                .keyboard
                .iter()
                .map(|path| load(path))
                .collect::<Result<_, _>>()?,
            left_up: load(&config.left_up)?,
            left_down: config
                .left_down
                .iter()
                .map(|path| load(path))
                .collect::<Result<_, _>>()?,
            right_up: load(&config.right_up)?,
            right_down: config
                .right_down
                .iter()
                .map(|path| load(path))
                .collect::<Result<_, _>>()?,
        })
    }

    /// 按固定顺序合成一帧：
    /// 清空画布、绘制背景、可选键盘高亮、左手层、右手层。
    pub fn render(&self, state: VisualState, target: &mut [u8]) {
        target.fill(0);
        blend(&self.background, self.width, self.height, target);
        if let Some(index) = state.highlight
            && let Some(layer) = self.keyboard.get(index)
        {
            blend(layer, self.width, self.height, target);
        }
        match state.left_pose {
            Some(index) if !self.left_down.is_empty() => blend(
                &self.left_down[index % self.left_down.len()],
                self.width,
                self.height,
                target,
            ),
            _ => blend(&self.left_up, self.width, self.height, target),
        }
        match state.right_pose {
            Some(index) if !self.right_down.is_empty() => blend(
                &self.right_down[index % self.right_down.len()],
                self.width,
                self.height,
                target,
            ),
            _ => blend(&self.right_up, self.width, self.height, target),
        }
    }
}

/// 加载单个 PNG 图层并转换为裁剪后的预乘 alpha 图层。
fn load_layer(
    path: &Path,
    target_width: u32,
    target_height: u32,
) -> Result<Layer, Box<dyn std::error::Error>> {
    let file = BufReader::new(File::open(path).map_err(|error| {
        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    })?);
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut bytes = vec![0; reader.output_buffer_size().ok_or("PNG is too large")?];
    let info = reader.next_frame(&mut bytes)?;
    let bytes = &bytes[..info.buffer_size()];
    let rgba = to_rgba(bytes, info.color_type)?;
    let scaled = scale_premultiplied(&rgba, info.width, info.height, target_width, target_height);
    Ok(trim_layer(scaled, target_width, target_height))
}

/// 把 PNG 解码后的像素统一转换为 RGBA 字节序。
fn to_rgba(bytes: &[u8], color: png::ColorType) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut out = Vec::with_capacity(match color {
        png::ColorType::Rgba => bytes.len(),
        png::ColorType::Rgb => bytes.len() / 3 * 4,
        png::ColorType::Grayscale => bytes.len() * 4,
        png::ColorType::GrayscaleAlpha => bytes.len() / 2 * 4,
        png::ColorType::Indexed => return Err("indexed PNG was not expanded".into()),
    });
    match color {
        png::ColorType::Rgba => out.extend_from_slice(bytes),
        png::ColorType::Rgb => {
            for pixel in bytes.chunks_exact(3) {
                out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &gray in bytes {
                out.extend_from_slice(&[gray, gray, gray, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in bytes.chunks_exact(2) {
                out.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Indexed => unreachable!(),
    }
    Ok(out)
}

/// 先预乘 alpha，再用双线性插值缩放图片。
///
/// 预乘后再插值可以避免透明边缘出现颜色光晕。
fn scale_premultiplied(source: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut premultiplied = vec![0; source.len()];
    for (source, target) in source
        .chunks_exact(4)
        .zip(premultiplied.chunks_exact_mut(4))
    {
        let alpha = source[3] as u16;
        target[0] = ((source[2] as u16 * alpha + 127) / 255) as u8;
        target[1] = ((source[1] as u16 * alpha + 127) / 255) as u8;
        target[2] = ((source[0] as u16 * alpha + 127) / 255) as u8;
        target[3] = source[3];
    }
    if sw == dw && sh == dh {
        return premultiplied;
    }

    let mut out = vec![0; dw as usize * dh as usize * 4];
    for y in 0..dh {
        let source_y = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = source_y.floor() as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = source_y - y0 as f32;
        for x in 0..dw {
            let source_x =
                ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = source_x.floor() as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = source_x - x0 as f32;
            let di = ((y * dw + x) * 4) as usize;
            let indices = [
                ((y0 * sw + x0) * 4) as usize,
                ((y0 * sw + x1) * 4) as usize,
                ((y1 * sw + x0) * 4) as usize,
                ((y1 * sw + x1) * 4) as usize,
            ];
            for channel in 0..4 {
                let top = premultiplied[indices[0] + channel] as f32 * (1.0 - fx)
                    + premultiplied[indices[1] + channel] as f32 * fx;
                let bottom = premultiplied[indices[2] + channel] as f32 * (1.0 - fx)
                    + premultiplied[indices[3] + channel] as f32 * fx;
                out[di + channel] = (top * (1.0 - fy) + bottom * fy).round() as u8;
            }
        }
    }
    out
}

/// 裁掉图层四周完全透明的区域，减少后续混合工作量。
fn trim_layer(pixels: Vec<u8>, width: u32, height: u32) -> Layer {
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    for y in 0..height {
        for x in 0..width {
            if pixels[((y * width + x) * 4 + 3) as usize] != 0 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if min_x == width {
        return Layer {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
    }
    let trimmed_width = max_x - min_x + 1;
    let trimmed_height = max_y - min_y + 1;
    let mut trimmed = Vec::with_capacity(trimmed_width as usize * trimmed_height as usize * 4);
    for y in min_y..=max_y {
        let start = ((y * width + min_x) * 4) as usize;
        let end = start + trimmed_width as usize * 4;
        trimmed.extend_from_slice(&pixels[start..end]);
    }
    Layer {
        x: min_x,
        y: min_y,
        width: trimmed_width,
        height: trimmed_height,
        pixels: trimmed,
    }
}

/// 将一个预乘 alpha 图层混合到目标缓冲区。
fn blend(layer: &Layer, canvas_width: u32, canvas_height: u32, target: &mut [u8]) {
    if layer.width == 0 {
        return;
    }
    debug_assert!(layer.x + layer.width <= canvas_width && layer.y + layer.height <= canvas_height);
    for y in 0..layer.height {
        for x in 0..layer.width {
            let si = ((y * layer.width + x) * 4) as usize;
            let di = (((y + layer.y) * canvas_width + x + layer.x) * 4) as usize;
            let alpha = layer.pixels[si + 3] as u16;
            if alpha == 0 {
                continue;
            }
            let inverse = 255 - alpha;
            for channel in 0..4 {
                target[di + channel] = (layer.pixels[si + channel] as u16
                    + (target[di + channel] as u16 * inverse + 127) / 255)
                    .min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_bounds_are_trimmed() {
        let mut pixels = vec![0; 4 * 3 * 4];
        pixels[(6 * 4 + 3) as usize] = 255;
        let layer = trim_layer(pixels, 4, 3);
        assert_eq!((layer.x, layer.y, layer.width, layer.height), (2, 1, 1, 1));
    }
}
