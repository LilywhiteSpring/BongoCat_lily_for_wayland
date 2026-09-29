//! 配置文件的数据结构和加载逻辑
//!
//! 配置文件采用 TOML 格式，通过 [`serde`] 反序列化为 [`Config`]
//! 素材路径如果是相对路径，会以配置文件所在目录为基准进行解析

use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// 完整的应用配置
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub window: WindowConfig,
    pub input: InputConfig,
    pub assets: AssetsConfig,
    pub bindings: BindingsConfig,
}

/// 悬浮窗口的尺寸、锚点和 layer 配置。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub anchor: String,
    pub layer: String,
    pub margin_x: i32,
    pub margin_y: i32,
}

/// 输入设备发现与重扫描配置。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub devices: Vec<PathBuf>,
    pub rescan_seconds: u64,
}

/// 素材文件路径配置。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AssetsConfig {
    pub root: PathBuf,
    pub background: PathBuf,
    pub keyboard: Vec<PathBuf>,
    pub left_up: PathBuf,
    pub left_down: Vec<PathBuf>,
    pub right_up: PathBuf,
    pub right_down: Vec<PathBuf>,
}

/// 按键绑定集合。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct BindingsConfig {
    pub keys: Vec<KeyBinding>,
}

/// 按键对应的动画手部。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Hand {
    Left,
    Right,
    Both,
}

/// 单个按键绑定。
///
/// `code` 是 Linux evdev 键名，`hand` 决定哪只手响应，
/// `pose` 决定手部图片索引，`highlight` 决定键盘高亮图片索引。
#[derive(Clone, Debug, Deserialize)]
pub struct KeyBinding {
    pub code: String,
    pub hand: Hand,
    #[serde(default)]
    pub pose: usize,
    pub highlight: Option<usize>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 450,
            height: 281,
            anchor: "bottom-right".into(),
            layer: "top".into(),
            margin_x: 24,
            margin_y: 12,
        }
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            rescan_seconds: 3,
        }
    }
}

impl Default for AssetsConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("assets/keyboard"),
            background: PathBuf::from("bg.png"),
            keyboard: (0..7).map(|n| format!("keyboard/{n}.png").into()).collect(),
            left_up: "lefthand/leftup.png".into(),
            left_down: [
                "lefthand/0.png",
                "lefthand/1.png",
                "lefthand/2.png",
                "lefthand/3k.png",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            right_up: "righthand/rightup.png".into(),
            right_down: (0..4)
                .map(|n| format!("righthand/{n}.png").into())
                .collect(),
        }
    }
}

impl Config {
    /// 从可选路径加载配置。
    ///
    /// 如果未提供路径，则使用内置默认配置；否则读取并解析 TOML。
    pub fn load(path: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&text)?;
        if config.assets.root.is_relative() {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            config.assets.root = parent.join(&config.assets.root);
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_source_canvas_aspect_ratio() {
        let cfg = Config::default();
        assert_eq!(cfg.window.width, 450);
        assert_eq!(cfg.window.height, 281);
        assert_eq!(cfg.assets.keyboard.len(), 7);
        assert_eq!(cfg.assets.left_down.len(), 4);
        assert_eq!(cfg.assets.right_down.len(), 4);
    }
}
