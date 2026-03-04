mod clipboard_content;
mod network;

use arboard::Clipboard;
use clap::Parser;
use clipboard_content::ClipboardContent;
use clipboard_master::{CallbackResult, ClipboardHandler, Master};
use env_logger::Env;
use log::{debug, error, info, warn};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// 全局共享的去重状态，防止回环
/// set_clipboard_content 写入后更新此状态，on_clipboard_change 读取时跳过相同内容
struct DeduplicationState {
    last_text: Option<String>,
    last_image_hash: Option<u64>,
}

impl DeduplicationState {
    fn new() -> Self {
        Self {
            last_text: None,
            last_image_hash: None,
        }
    }
}

type SharedDedup = Arc<Mutex<DeduplicationState>>;

struct Handler {
    sender: mpsc::Sender<ClipboardContent>,
    dedup: SharedDedup,
}

impl ClipboardHandler for Handler {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        debug!("Clipboard change happened!");
        get_clipboard_content(self.sender.clone(), self.dedup.clone());
        CallbackResult::Next
    }

    fn on_clipboard_error(&mut self, error: io::Error) -> CallbackResult {
        error!("Clipboard monitor error: {}", error);
        CallbackResult::Next
    }

    fn sleep_interval(&self) -> core::time::Duration {
        core::time::Duration::from_millis(1000)
    }
}

#[derive(Parser, Debug)]
#[command(version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = env!("CARGO_PKG_DESCRIPTION"))]
struct Args {
    /// The remote peer to connect to on boot up.
    #[arg(short, long, num_args = 2, value_names = ["IP:PORT", "PEER_ID"])]
    connect: Option<Vec<String>>,
    /// Path to custom private key. The key should be an ED25519 private key in PEM format.
    #[arg(short, long, value_name = "PATH")]
    key: Option<String>,
    /// Local address to listen on.
    #[arg(short, long, value_name = "IP:PORT")]
    listen: Option<String>,
    /// Pre-shared key. Only nodes with same key can connect to each other.
    #[arg(short, long)]
    psk: Option<String>,
    /// If set, no mDNS broadcasts will be made.
    #[arg(short, long)]
    no_mdns: bool,
}

fn hash_bytes(data: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

fn get_clipboard_content(sender: mpsc::Sender<ClipboardContent>, dedup: SharedDedup) {
    let mut ctx = match Clipboard::new() {
        Ok(context) => context,
        Err(err) => {
            error!("Error creating ClipboardContext: {}", err);
            return;
        }
    };

    // 优先尝试获取图片
    match ctx.get_image() {
        Ok(img_data) => {
            let h = hash_bytes(&img_data.bytes);
            let mut guard = match dedup.lock() {
                Ok(g) => g,
                Err(e) => {
                    error!("Failed to lock dedup state: {}", e);
                    return;
                }
            };
            if guard.last_image_hash == Some(h) {
                debug!("Image unchanged (dedup hit), skipping");
                return;
            }
            guard.last_image_hash = Some(h);
            guard.last_text = None;
            drop(guard);

            let content = ClipboardContent::Image {
                width: img_data.width,
                height: img_data.height,
                bytes: img_data.bytes.into_owned(),
            };
            debug!("Clipboard image: {}", content.description());
            if let Err(e) = sender.try_send(content) {
                error!("Failed to send clipboard image: {}", e);
            }
            return;
        }
        Err(_) => {
            // 不是图片，继续尝试文本
        }
    }

    // 尝试获取文本
    match ctx.get_text() {
        Ok(contents) => {
            let mut guard = match dedup.lock() {
                Ok(g) => g,
                Err(e) => {
                    error!("Failed to lock dedup state: {}", e);
                    return;
                }
            };
            if guard.last_text.as_deref() == Some(contents.as_str()) {
                debug!("Text unchanged (dedup hit), skipping");
                return;
            }
            guard.last_text = Some(contents.clone());
            guard.last_image_hash = None;
            drop(guard);

            // 检查文本是否是文件路径列表（支持文件复制同步）
            let lines: Vec<&str> = contents.lines().collect();
            if !lines.is_empty()
                && lines.iter().all(|l| {
                    let trimmed = l.trim();
                    if trimmed.is_empty() {
                        return false;
                    }
                    let p = PathBuf::from(trimmed);
                    p.is_absolute() && p.exists() && p.is_file()
                })
            {
                for line in &lines {
                    let path = PathBuf::from(line.trim());
                    let file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    match std::fs::read(&path) {
                        Ok(file_bytes) => {
                            let content = ClipboardContent::File {
                                name: file_name.clone(),
                                bytes: file_bytes,
                            };
                            info!("Sending file: {} ({} bytes)", file_name, content.description());
                            if let Err(e) = sender.try_send(content) {
                                error!("Failed to send file {}: {}", file_name, e);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to read file {}: {}", line.trim(), e);
                        }
                    }
                }
                return;
            }

            let content = ClipboardContent::Text(contents);
            if let Err(e) = sender.try_send(content) {
                error!("Failed to send clipboard text: {}", e);
            }
        }
        Err(err) => error!("Error getting clipboard contents: {}", err),
    }
}

/// 将远端收到的内容写入本地剪贴板，同时更新 dedup 状态防止回环
fn set_clipboard_content(content: &ClipboardContent, dedup: &SharedDedup) {
    let mut ctx = match Clipboard::new() {
        Ok(context) => context,
        Err(err) => {
            error!("Error creating ClipboardContext: {}", err);
            return;
        }
    };
    match content {
        ClipboardContent::Text(text) => {
            // 先更新 dedup 状态，再写入剪贴板，防止回环
            if let Ok(mut guard) = dedup.lock() {
                guard.last_text = Some(text.clone());
                guard.last_image_hash = None;
            }
            if let Err(e) = ctx.set_text(text) {
                error!("Error setting clipboard text: {}", e);
            }
        }
        ClipboardContent::Image { width, height, bytes } => {
            let h = hash_bytes(bytes);
            if let Ok(mut guard) = dedup.lock() {
                guard.last_image_hash = Some(h);
                guard.last_text = None;
            }
            let img = arboard::ImageData {
                width: *width,
                height: *height,
                bytes: Cow::Borrowed(bytes.as_slice()),
            };
            if let Err(e) = ctx.set_image(img) {
                error!("Error setting clipboard image: {}", e);
            }
        }
        ClipboardContent::File { name, bytes } => {
            let dest_dir = get_file_receive_dir();
            if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                error!("Failed to create receive dir {:?}: {}", dest_dir, e);
                return;
            }
            let dest_path = dest_dir.join(name);
            match std::fs::write(&dest_path, bytes) {
                Ok(_) => {
                    info!("File received and saved to: {:?}", dest_path);
                    let path_str = dest_path.to_string_lossy().to_string();
                    // 更新 dedup 状态防止回环
                    if let Ok(mut guard) = dedup.lock() {
                        guard.last_text = Some(path_str.clone());
                        guard.last_image_hash = None;
                    }
                    if let Err(e) = ctx.set_text(&path_str) {
                        error!("Error setting file path to clipboard: {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to write file {:?}: {}", dest_path, e);
                }
            }
        }
    }
}

/// 获取文件接收目录：Windows 为 ~/temp，Linux 为 /tmp
fn get_file_receive_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("temp")
    } else {
        PathBuf::from("/tmp")
    }
}

fn create_clipboard_monitor(
    sender: mpsc::Sender<ClipboardContent>,
    dedup: SharedDedup,
) -> Result<Master<Handler>, io::Error> {
    let handler = Handler { sender, dedup };
    Master::new(handler)
}

async fn channel_proxy(
    mut rx: mpsc::Receiver<ClipboardContent>,
    dedup: SharedDedup,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            Some(message) = rx.recv() => {
                debug!("Proxy received: {}", message.description());
                set_clipboard_content(&message, &dedup);
            },
            _ = &mut shutdown => {
                debug!("Proxy shutdown received");
                return;
            },
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stdout)
        .init();
    let Args {
        connect,
        key,
        listen,
        psk,
        no_mdns,
    } = Args::parse();
    loop {
        let dedup: SharedDedup = Arc::new(Mutex::new(DeduplicationState::new()));

        let (from_clipboard_tx, from_clipboard_rx) = mpsc::channel::<ClipboardContent>(32);
        let (to_clipboard_tx, to_clipboard_rx) = mpsc::channel::<ClipboardContent>(32);
        let (shutdown_proxy_tx, shutdown_proxy_rx) = oneshot::channel::<()>();
        let (shutdown_channel_tx, shutdown_channel_rx) = oneshot::channel();

        let proxy_dedup = dedup.clone();
        let _ = tokio::spawn(channel_proxy(to_clipboard_rx, proxy_dedup, shutdown_proxy_rx));

        let monitor_dedup = dedup.clone();
        std::thread::spawn(move || {
            let mut monitor = match create_clipboard_monitor(from_clipboard_tx, monitor_dedup) {
                Ok(m) => m,
                Err(e) => {
                    error!("Failed to create clipboard monitor: {}", e);
                    return;
                }
            };
            let shutdown = monitor.shutdown_channel();
            let _ = shutdown_channel_tx.send(shutdown);
            if let Err(e) = monitor.run() {
                error!("Clipboard monitor error: {}", e);
            }
        });

        let result = network::start_network(
            from_clipboard_rx,
            to_clipboard_tx,
            connect.clone(),
            key.clone(),
            listen.clone(),
            psk.clone(),
            no_mdns,
        )
        .await;
        if let Err(error_in_network) = result {
            error!("Fatal Error: {}", error_in_network);
            std::process::exit(1);
        }
        if let Ok(shutdown) = shutdown_channel_rx.await {
            shutdown.signal();
        }
        let _ = shutdown_proxy_tx.send(());
    }
}
