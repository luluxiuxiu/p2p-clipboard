use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 单个分片的最大数据大小（1.5MB，留余量给元数据和压缩开销）
pub const CHUNK_DATA_SIZE: usize = 1_500_000;

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
    /// 多文件列表（从文件管理器复制）
    Files(Vec<FileEntry>),
}

/// 单个文件条目
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// 网络传输消息，包含普通消息和分片消息
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum NetworkMessage {
    /// 小数据直接传输（兼容旧版）
    Direct(ClipboardContent),
    /// 分片传输：起始通知
    ChunkStart {
        /// 本次传输的唯一 ID
        transfer_id: String,
        /// 总分片数
        total_chunks: u32,
        /// 原始数据的 SHA-256 哈希（hex）
        data_hash: String,
        /// 原始数据总大小
        total_size: u64,
    },
    /// 分片传输：单个数据块
    Chunk {
        transfer_id: String,
        index: u32,
        data: Vec<u8>,
    },
    /// 分片传输：结束确认
    ChunkEnd {
        transfer_id: String,
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
            ClipboardContent::Files(files) => {
                let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                format!("Files({} files: {:?})", files.len(), names)
            }
        }
    }
}

impl NetworkMessage {
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

/// 计算数据的 SHA-256 哈希（返回 hex 字符串）
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(result)
}

/// 将 ClipboardContent 拆分为 NetworkMessage 序列
/// 如果数据小于 CHUNK_DATA_SIZE，直接返回 Direct 消息
/// 否则返回 ChunkStart + N * Chunk + ChunkEnd
pub fn split_to_network_messages(content: &ClipboardContent) -> Result<Vec<NetworkMessage>, bincode::Error> {
    let raw = content.to_bytes()?;
    if raw.len() <= CHUNK_DATA_SIZE {
        return Ok(vec![NetworkMessage::Direct(content.clone())]);
    }

    let data_hash = sha256_hex(&raw);
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let total_chunks = ((raw.len() + CHUNK_DATA_SIZE - 1) / CHUNK_DATA_SIZE) as u32;

    let mut messages = Vec::with_capacity(total_chunks as usize + 2);

    messages.push(NetworkMessage::ChunkStart {
        transfer_id: transfer_id.clone(),
        total_chunks,
        data_hash,
        total_size: raw.len() as u64,
    });

    for (i, chunk) in raw.chunks(CHUNK_DATA_SIZE).enumerate() {
        messages.push(NetworkMessage::Chunk {
            transfer_id: transfer_id.clone(),
            index: i as u32,
            data: chunk.to_vec(),
        });
    }

    messages.push(NetworkMessage::ChunkEnd {
        transfer_id: transfer_id.clone(),
    });

    Ok(messages)
}
