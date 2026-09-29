//! 库入口

//! 本库只负责组织各功能模块，实际的 Wayland 应用生命周期在[`main`] 所在的二进制 crate 中完成。

pub mod config;
pub mod graphics;
pub mod input;
