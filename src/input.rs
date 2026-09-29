//! Linux evdev 输入读取、设备扫描和按键映射。
//!
//! Wayland 不向普通客户端提供全局键盘输入，因此这里直接从
//! `/dev/input/event*` 读取物理按键和鼠标按钮事件，再通过
//! [`Sender`] 发送到主事件循环。

use crate::config::{BindingsConfig, Hand, KeyBinding};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const EV_KEY: u16 = 1;
const BTN_LEFT: u16 = 272;
const BTN_RIGHT: u16 = 273;
const BTN_MIDDLE: u16 = 274;

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxInputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

/// 从输入线程发送给主线程的消息。
#[derive(Clone, Debug)]
pub enum InputMessage {
    Key {
        device: u64,
        code: u16,
        pressed: bool,
    },
    Reset,
}

/// 一个输入码对应的动画动作。
#[derive(Clone, Copy, Debug)]
pub struct Action {
    pub hand: Hand,
    pub pose: usize,
    pub highlight: Option<usize>,
}

/// 输入码到动画动作的映射表。
///
/// 优先使用配置文件中的显式绑定；未命中时再应用内置的通用规则。
pub struct Mapping {
    explicit: HashMap<u16, Action>,
}

impl Mapping {
    /// 根据绑定配置构造映射表。
    pub fn new(config: &BindingsConfig) -> Self {
        let source = if config.keys.is_empty() {
            default_bindings()
        } else {
            config.keys.clone()
        };
        let explicit = source
            .into_iter()
            .filter_map(|binding| {
                parse_key_code(&binding.code).map(|code| {
                    (
                        code,
                        Action {
                            hand: binding.hand,
                            pose: binding.pose,
                            highlight: binding.highlight,
                        },
                    )
                })
            })
            .collect();
        Self { explicit }
    }

    /// 返回某个 Linux 输入码对应的动作。
    pub fn action(&self, code: u16) -> Option<Action> {
        self.explicit
            .get(&code)
            .copied()
            .or_else(|| generic_action(code))
    }
}

/// 管理后台输入读取线程。
pub struct InputWorker {
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputWorker {
    /// 启动输入线程，返回用于控制线程生命周期的句柄。
    pub fn spawn(paths: Vec<PathBuf>, rescan_seconds: u64, sender: Sender<InputMessage>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let thread = thread::spawn(move || {
            run_input_loop(
                paths,
                Duration::from_secs(rescan_seconds.max(1)),
                sender,
                worker_running,
            )
        });
        Self {
            running,
            thread: Some(thread),
        }
    }
}

impl Drop for InputWorker {
    /// 停止后台线程并等待其退出。
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 后台输入循环：定期发现设备、`poll` 等待事件并发送按键消息。
fn run_input_loop(
    configured: Vec<PathBuf>,
    interval: Duration,
    sender: Sender<InputMessage>,
    running: Arc<AtomicBool>,
) {
    let mut current_paths = Vec::new();
    let mut devices: Vec<(u64, PathBuf, File)> = Vec::new();
    let mut next_id = 1_u64;

    while running.load(Ordering::Relaxed) {
        let paths = discover_paths(&configured);
        if paths != current_paths {
            devices.clear();
            for path in &paths {
                match OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                {
                    Ok(file) => {
                        devices.push((next_id, path.clone(), file));
                        next_id += 1;
                    }
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                        eprintln!("no permission to read {}", path.display());
                    }
                    Err(_) => {}
                }
            }
            current_paths = paths;
            if sender.send(InputMessage::Reset).is_err() {
                break;
            }
        }

        let mut poll_fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|(_, _, file)| libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let timeout = i32::try_from(interval.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: poll_fds points to initialized pollfd values for the duration of poll.
        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, timeout) };
        if ready <= 0 {
            continue;
        }

        for (index, poll_fd) in poll_fds.iter().enumerate() {
            if poll_fd.revents & libc::POLLIN == 0 {
                continue;
            }
            let (device_id, _, file) = &devices[index];
            loop {
                let mut event = std::mem::MaybeUninit::<LinuxInputEvent>::uninit();
                // SAFETY: event is valid writable storage and the exact byte count is checked.
                let count = unsafe {
                    libc::read(
                        file.as_raw_fd(),
                        event.as_mut_ptr().cast(),
                        std::mem::size_of::<LinuxInputEvent>(),
                    )
                };
                if count != std::mem::size_of::<LinuxInputEvent>() as isize {
                    break;
                }
                // SAFETY: read initialized the complete structure.
                let event = unsafe { event.assume_init() };
                if event.kind == EV_KEY
                    && (event.value == 0 || event.value == 1)
                    && sender
                        .send(InputMessage::Key {
                            device: *device_id,
                            code: event.code,
                            pressed: event.value == 1,
                        })
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// 根据配置发现需要监听的输入设备路径。
///
/// 配置为空时扫描 `/dev/input/event*`；否则只保留配置中实际存在的路径。
fn discover_paths(configured: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = if configured.is_empty() {
        fs::read_dir("/dev/input")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("event"))
            })
            .collect()
    } else {
        configured
            .iter()
            .filter(|path| Path::new(path).exists())
            .cloned()
            .collect()
    };
    paths.sort();
    paths
}

/// 返回程序内置的默认按键绑定。
fn default_bindings() -> Vec<KeyBinding> {
    use Hand::*;
    [
        ("KEY_UP", Right, 0, 6),
        ("KEY_LEFT", Right, 1, 4),
        ("KEY_DOWN", Right, 3, 3),
        ("KEY_RIGHT", Right, 2, 5),
        ("KEY_X", Left, 2, 2),
        ("KEY_Z", Left, 1, 1),
        ("KEY_LEFTSHIFT", Left, 0, 0),
    ]
    .into_iter()
    .map(|(code, hand, pose, highlight)| KeyBinding {
        code: code.into(),
        hand,
        pose,
        highlight: Some(highlight),
    })
    .collect()
}

/// 为未在配置中显式绑定的输入码生成通用动作。
///
/// 鼠标左/右/中键以及空格有特殊处理，其他键盘键按 QWERTY 物理位置
/// 粗略划分到左手或右手。
fn generic_action(code: u16) -> Option<Action> {
    if code == BTN_LEFT {
        return Some(Action {
            hand: Hand::Left,
            pose: 0,
            highlight: None,
        });
    }
    if code == BTN_RIGHT {
        return Some(Action {
            hand: Hand::Right,
            pose: 0,
            highlight: None,
        });
    }
    if code == BTN_MIDDLE || code == 57 {
        return Some(Action {
            hand: Hand::Both,
            pose: 0,
            highlight: None,
        });
    }
    if !(1..=255).contains(&code) {
        return None;
    }
    let left_side = matches!(code,
        2..=6 | 16..=20 | 30..=34 | 44..=48 | 29 | 42 | 56 | 125);
    Some(Action {
        hand: if left_side { Hand::Left } else { Hand::Right },
        pose: code as usize,
        highlight: None,
    })
}

/// 将配置中的键名或数字字符串解析为 Linux evdev 输入码。
pub fn parse_key_code(name: &str) -> Option<u16> {
    if let Ok(code) = name.parse() {
        return Some(code);
    }
    let code = match name {
        "KEY_ESC" => 1,
        "KEY_1" => 2,
        "KEY_2" => 3,
        "KEY_3" => 4,
        "KEY_4" => 5,
        "KEY_5" => 6,
        "KEY_6" => 7,
        "KEY_7" => 8,
        "KEY_8" => 9,
        "KEY_9" => 10,
        "KEY_0" => 11,
        "KEY_Q" => 16,
        "KEY_W" => 17,
        "KEY_E" => 18,
        "KEY_R" => 19,
        "KEY_T" => 20,
        "KEY_Y" => 21,
        "KEY_U" => 22,
        "KEY_I" => 23,
        "KEY_O" => 24,
        "KEY_P" => 25,
        "KEY_ENTER" => 28,
        "KEY_LEFTCTRL" => 29,
        "KEY_A" => 30,
        "KEY_S" => 31,
        "KEY_D" => 32,
        "KEY_F" => 33,
        "KEY_G" => 34,
        "KEY_H" => 35,
        "KEY_J" => 36,
        "KEY_K" => 37,
        "KEY_L" => 38,
        "KEY_LEFTSHIFT" => 42,
        "KEY_Z" => 44,
        "KEY_X" => 45,
        "KEY_C" => 46,
        "KEY_V" => 47,
        "KEY_B" => 48,
        "KEY_N" => 49,
        "KEY_M" => 50,
        "KEY_RIGHTSHIFT" => 54,
        "KEY_LEFTALT" => 56,
        "KEY_SPACE" => 57,
        "KEY_RIGHTCTRL" => 97,
        "KEY_RIGHTALT" => 100,
        "KEY_UP" => 103,
        "KEY_LEFT" => 105,
        "KEY_RIGHT" => 106,
        "KEY_DOWN" => 108,
        "KEY_LEFTMETA" => 125,
        "KEY_RIGHTMETA" => 126,
        "BTN_LEFT" => BTN_LEFT,
        "BTN_RIGHT" => BTN_RIGHT,
        "BTN_MIDDLE" => BTN_MIDDLE,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keyboard_splits_hands_and_highlights() {
        let mapping = Mapping::new(&BindingsConfig::default());
        let expected = [
            ("KEY_UP", Hand::Right, 0, 6),
            ("KEY_LEFT", Hand::Right, 1, 4),
            ("KEY_DOWN", Hand::Right, 3, 3),
            ("KEY_RIGHT", Hand::Right, 2, 5),
            ("KEY_X", Hand::Left, 2, 2),
            ("KEY_Z", Hand::Left, 1, 1),
            ("KEY_LEFTSHIFT", Hand::Left, 0, 0),
        ];
        for (key, hand, pose, highlight) in expected {
            let action = mapping.action(parse_key_code(key).unwrap()).unwrap();
            assert_eq!(action.hand, hand, "wrong hand for {key}");
            assert_eq!(action.pose, pose, "wrong pose for {key}");
            assert_eq!(
                action.highlight,
                Some(highlight),
                "wrong highlight for {key}"
            );
        }

        assert_eq!(
            mapping
                .action(parse_key_code("KEY_Q").unwrap())
                .unwrap()
                .hand,
            Hand::Left
        );
        assert_eq!(
            mapping
                .action(parse_key_code("KEY_Y").unwrap())
                .unwrap()
                .hand,
            Hand::Right
        );
    }
}
