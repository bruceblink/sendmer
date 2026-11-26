//! iroh endpoint、ticket 地址和 relay 连接入口。
//!
//! 传输流程通过本模块创建 endpoint，不向 CLI 或桌面客户端暴露底层构造细节。

mod endpoint;
mod identity;

pub use endpoint::base_endpoint_builder;
pub use identity::endpoint_secret;
