//! 领域核心类型定义。
//!
//! 本文件只保留发送/接收共享的领域类型，避免与参数、选项、结果等模块重复。

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

/// Entry type for transfers (file or directory)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryType {
    File,
    Directory,
}

impl EntryType {
    pub const fn min_required_transfers(&self) -> usize {
        match self {
            Self::File => 1,
            Self::Directory => 2,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

impl Display for EntryType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
