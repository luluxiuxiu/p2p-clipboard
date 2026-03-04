use serde::{Deserialize, Serialize};

/// 剪贴板内容的统一类型，支持文本、图片和文件
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClipboardContent {
    /// 纯文本内容
    Text(String),
    /// 图片数据：宽度、高度、RGBA 像素字节
    Image {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
    /// 文件内容：文件名、文件字节
    File {
        name: String,
        bytes: Vec<u8>,
    },
}

impl ClipboardContent {
    /// 序列化为字节
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// 从字节反序列化
    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }

    /// 获取内容的描述信息（用于日志）
    pub fn description(&self) -> String {
        match self {
            ClipboardContent::Text(s) => {
                let preview: String = s.chars().take(50).collect();
                format!("Text({} bytes, preview: \"{}...\")", s.len(), preview)
            }
            ClipboardContent::Image { width, height, bytes } => {
                format!("Image({}x{}, {} bytes)", width, height, bytes.len())
            }
            ClipboardContent::File { name, bytes } => {
                format!("File(\"{}\", {} bytes)", name, bytes.len())
            }
        }
    }
}
