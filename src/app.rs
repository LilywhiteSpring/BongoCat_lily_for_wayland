//! Wayland 应用状态与事件处理。
//!
//! 这个模块保存当前按下的按键、待渲染状态、`wl_shm` 缓冲池和layer-shell surface，并实现 SCTK 所需的各协议 handler

use bongocat_lily_wayland::{
    config::{Config, Hand},
    graphics::{Scene, VisualState},
    input::{Action, InputMessage, Mapping},
};
use smithay_client_toolkit::{
    compositor::CompositorHandler,
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, Layer, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::collections::HashMap;
use wayland_client::{
    Connection, QueueHandle,
    protocol::{wl_output, wl_shm, wl_surface},
};

/// 主应用状态机

/// 它同时承担Wayland事件处理和输入驱动的重绘调度
pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pub layer: LayerSurface,
    pub exit: bool,
    configured: bool,
    width: u32,
    height: u32,
    pool: SlotPool,
    scene: Scene,
    mapping: Mapping,
    pressed: HashMap<(u64, u16), u64>,
    sequence: u64,
    dirty: bool,
    frame_pending: bool,
    queue: QueueHandle<App>,
}

impl App {
    /// 创建应用状态。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry_state: RegistryState,
        output_state: OutputState,
        shm: Shm,
        layer: LayerSurface,
        pool: SlotPool,
        scene: Scene,
        config: &Config,
        queue: QueueHandle<App>,
    ) -> Self {
        Self {
            registry_state,
            output_state,
            shm,
            layer,
            exit: false,
            configured: false,
            width: config.window.width,
            height: config.window.height,
            pool,
            scene,
            mapping: Mapping::new(&config.bindings),
            pressed: HashMap::new(),
            sequence: 0,
            dirty: true,
            frame_pending: false,
            queue,
        }
    }

    /// 处理来自输入线程的消息，并在需要时触发重绘
    pub fn handle_input(&mut self, message: InputMessage) {
        match message {
            InputMessage::Reset => self.pressed.clear(),
            InputMessage::Key {
                device,
                code,
                pressed,
            } => {
                if pressed {
                    self.sequence = self.sequence.wrapping_add(1);
                    self.pressed.insert((device, code), self.sequence);
                } else {
                    self.pressed.remove(&(device, code));
                }
            }
        }
        self.dirty = true;
        if self.configured && !self.frame_pending {
            self.draw();
        }
    }

    /// 根据当前按下的键计算需要展示的视觉状态
    ///
    /// 同一只手的多个动作会按“最后按下”的顺序选择最新的那个

    fn visual_state(&self) -> VisualState {
        let mut left: Option<(u64, Action)> = None;
        let mut right: Option<(u64, Action)> = None;
        let mut latest_highlight: Option<(u64, usize)> = None;
        for ((_, code), sequence) in &self.pressed {
            let Some(action) = self.mapping.action(*code) else {
                continue;
            };
            if let Some(highlight) = action.highlight
                && latest_highlight.is_none_or(|(old, _)| *sequence > old)
            {
                latest_highlight = Some((*sequence, highlight));
            }
            match action.hand {
                Hand::Left => {
                    if left.is_none_or(|(old, _)| *sequence > old) {
                        left = Some((*sequence, action));
                    }
                }
                Hand::Right => {
                    if right.is_none_or(|(old, _)| *sequence > old) {
                        right = Some((*sequence, action));
                    }
                }
                Hand::Both => {
                    if left.is_none_or(|(old, _)| *sequence > old) {
                        left = Some((*sequence, action));
                    }
                    if right.is_none_or(|(old, _)| *sequence > old) {
                        right = Some((*sequence, action));
                    }
                }
            }
        }
        VisualState {
            left_pose: left.map(|(_, action)| action.pose),
            right_pose: right.map(|(_, action)| action.pose),
            highlight: latest_highlight.map(|(_, highlight)| highlight),
        }
    }

    /// 申请一个 `wl_shm` 缓冲区，渲染当前帧并提交给合成器
    ///
    /// 提交后会等待 frame callback，避免输入频繁时重复绘制
    ///
    fn draw(&mut self) {
        let stride = self.width as i32 * 4;
        let state = self.visual_state();
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            self.width as i32,
            self.height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            return;
        };
        self.scene.render(state, canvas);
        self.layer
            .wl_surface()
            .frame(&self.queue, self.layer.wl_surface().clone());
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, self.width as i32, self.height as i32);
        if buffer.attach_to(self.layer.wl_surface()).is_ok() {
            self.dirty = false;
            self.frame_pending = true;
            self.layer.commit();
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        if self.dirty {
            self.draw();
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if !self.configured {
            self.configured = true;
            self.draw();
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

/// 把配置中的 anchor 字符串转换为 layer-shell 的 [`Anchor`]。
pub fn parse_anchor(value: &str) -> Anchor {
    let mut anchor = Anchor::empty();
    if value.contains("top") {
        anchor |= Anchor::TOP;
    }
    if value.contains("bottom") {
        anchor |= Anchor::BOTTOM;
    }
    if value.contains("left") {
        anchor |= Anchor::LEFT;
    }
    if value.contains("right") {
        anchor |= Anchor::RIGHT;
    }
    anchor
}

/// 把配置中的 layer 字符串转换为 layer-shell 的 [`Layer`]。
pub fn parse_layer(value: &str) -> Layer {
    match value {
        "background" => Layer::Background,
        "bottom" => Layer::Bottom,
        "overlay" => Layer::Overlay,
        _ => Layer::Top,
    }
}
