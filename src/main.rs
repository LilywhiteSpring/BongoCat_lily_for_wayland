//! 可执行程序入口。
//!
//! 这个文件完成从“命令行参数”到“Wayland 悬浮层应用”的启动流程：
//! 加载配置、准备渲染场景、连接 Wayland 合成器、创建 layer-shell
//! 悬浮层，并启动独立输入线程。

mod app;

use app::{App, parse_anchor, parse_layer};
use bongocat_lily_wayland::{
    config::Config,
    graphics::Scene,
    input::{InputMessage, InputWorker},
};
use smithay_client_toolkit::{
    compositor::{CompositorState, Region},
    output::OutputState,
    reexports::{
        calloop::{
            EventLoop,
            channel::{Event, channel},
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::RegistryState,
    shell::{
        WaylandSurface,
        wlr_layer::{KeyboardInteractivity, LayerShell},
    },
    shm::{Shm, slot::SlotPool},
};
use std::{env, path::PathBuf};
use wayland_client::{Connection, globals::registry_queue_init};

/// 程序入口，只负责把错误打印到 stderr 并设置退出码。
fn main() {
    if let Err(error) = run() {
        eprintln!("bongocat-lily-wayland: {error}");
        std::process::exit(1);
    }
}

/// 执行完整的启动流程。
///
/// 返回 [`Err`] 时，[`main`] 会把错误信息输出到标准错误。
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_args()?;
    let mut config = Config::load(options.config.as_deref())?;
    options.apply(&mut config)?;
    let scene = Scene::load(&config.assets, config.window.width, config.window.height)?;
    if options.check_only {
        println!(
            "configuration and artwork are valid ({}x{}, root: {})",
            config.window.width,
            config.window.height,
            config.assets.root.display()
        );
        return Ok(());
    }

    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn)?;
    let queue = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &queue)?;
    let layer_shell = LayerShell::bind(&globals, &queue)?;
    let shm = Shm::bind(&globals, &queue)?;
    let surface = compositor.create_surface(&queue);
    let input_region = Region::new(&compositor)?;
    surface.set_input_region(Some(input_region.wl_region()));
    let layer = layer_shell.create_layer_surface(
        &queue,
        surface,
        parse_layer(&config.window.layer),
        Some("bongocat-lily"),
        None,
    );
    layer.set_anchor(parse_anchor(&config.window.anchor));
    layer.set_size(config.window.width, config.window.height);
    layer.set_margin(
        if config.window.anchor.contains("top") {
            config.window.margin_y
        } else {
            0
        },
        if config.window.anchor.contains("right") {
            config.window.margin_x
        } else {
            0
        },
        if config.window.anchor.contains("bottom") {
            config.window.margin_y
        } else {
            0
        },
        if config.window.anchor.contains("left") {
            config.window.margin_x
        } else {
            0
        },
    );
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let frame_bytes = config.window.width as usize * config.window.height as usize * 4;
    let pool = SlotPool::new(frame_bytes * 2, &shm)?;
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), event_queue).insert(event_loop.handle())?;
    let (sender, input_channel) = channel::<InputMessage>();
    event_loop
        .handle()
        .insert_source(input_channel, |event, _, app| match event {
            Event::Msg(message) => app.handle_input(message),
            Event::Closed => app.exit = true,
        })?;
    let _input = InputWorker::spawn(
        config.input.devices.clone(),
        config.input.rescan_seconds,
        sender,
    );
    let mut app = App::new(
        RegistryState::new(&globals),
        OutputState::new(&globals, &queue),
        shm,
        layer,
        pool,
        scene,
        &config,
        queue,
    );
    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }
    Ok(())
}

#[derive(Default)]
/// 命令行参数的可选覆盖项。
struct Options {
    config: Option<PathBuf>,
    check_only: bool,
    size: Option<(u32, u32)>,
    scale: Option<f32>,
    position: Option<String>,
    offset_x: Option<i32>,
    offset_y: Option<i32>,
    layer: Option<String>,
}

impl Options {
    /// 将命令行覆盖项应用到已加载的配置上。
    ///
    /// 会检查尺寸、缩放比例、位置和 layer 名称是否合法。
    fn apply(&self, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
        if let Some((width, height)) = self.size {
            config.window.width = width;
            config.window.height = height;
        }
        if let Some(scale) = self.scale {
            if !scale.is_finite() || !(0.1..=8.0).contains(&scale) {
                return Err("--scale must be between 0.1 and 8.0".into());
            }
            config.window.width = (config.window.width as f32 * scale).round() as u32;
            config.window.height = (config.window.height as f32 * scale).round() as u32;
        }
        if !(16..=8192).contains(&config.window.width)
            || !(16..=8192).contains(&config.window.height)
        {
            return Err("window dimensions must be between 16 and 8192 pixels".into());
        }
        if let Some(position) = &self.position {
            const POSITIONS: [&str; 9] = [
                "top-left",
                "top",
                "top-right",
                "left",
                "center",
                "right",
                "bottom-left",
                "bottom",
                "bottom-right",
            ];
            if !POSITIONS.contains(&position.as_str()) {
                return Err(format!("invalid --position: {position}").into());
            }
            config.window.anchor.clone_from(position);
        }
        if let Some(offset) = self.offset_x {
            config.window.margin_x = offset;
        }
        if let Some(offset) = self.offset_y {
            config.window.margin_y = offset;
        }
        if let Some(layer) = &self.layer {
            if !["background", "bottom", "top", "overlay"].contains(&layer.as_str()) {
                return Err(format!("invalid --layer: {layer}").into());
            }
            config.window.layer.clone_from(layer);
        }
        Ok(())
    }
}

/// 解析命令行参数。
///
/// 目前采用简单的手写解析器，支持 `-c/--config`、`--check`、
/// `--size`、`--scale`、`--position`、`--offset-x`、`--offset-y`
/// 和 `--layer`。
fn parse_args() -> Result<Options, Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-c" | "--config") => {
                options.config = Some(args.next().ok_or("--config needs a path")?.into())
            }
            Some("--check") => options.check_only = true,
            Some("--size") => {
                let value = args.next().ok_or("--size needs WIDTHxHEIGHT")?;
                let value = value.to_string_lossy();
                let (width, height) = value.split_once('x').ok_or("--size needs WIDTHxHEIGHT")?;
                options.size = Some((width.parse()?, height.parse()?));
            }
            Some("--scale") => {
                options.scale = Some(
                    args.next()
                        .ok_or("--scale needs a number")?
                        .to_string_lossy()
                        .parse()?,
                )
            }
            Some("--position") => {
                options.position = Some(
                    args.next()
                        .ok_or("--position needs an anchor")?
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            Some("--offset-x") => {
                options.offset_x = Some(
                    args.next()
                        .ok_or("--offset-x needs an integer")?
                        .to_string_lossy()
                        .parse()?,
                )
            }
            Some("--offset-y") => {
                options.offset_y = Some(
                    args.next()
                        .ok_or("--offset-y needs an integer")?
                        .to_string_lossy()
                        .parse()?,
                )
            }
            Some("--layer") => {
                options.layer = Some(
                    args.next()
                        .ok_or("--layer needs a value")?
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            Some("-h" | "--help") => {
                println!(
                    "Usage: bongocat-lily-wayland [OPTIONS]\n\n\
                     Options:\n  -c, --config FILE\n      --size WIDTHxHEIGHT\n\
                     \n      --scale FACTOR\n      --position ANCHOR\n\
                     \n      --offset-x PIXELS\n      --offset-y PIXELS\n\
                     \n      --layer background|bottom|top|overlay\n      --check\n\
                     \n  -h, --help\n\nGlobal input requires read access to /dev/input/event*."
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {}", arg.to_string_lossy()).into()),
        }
    }
    Ok(options)
}
