//! TM1 manifest 模型、路径编码和校验。
//!
//! 清单模块只处理协议数据和路径安全。文件导入和原子导出由 transfer 与 filesystem 负责。

mod codec;
mod model;
mod validate;

pub use model::*;
pub(crate) use validate::validate_windows_utf8_component;
#[cfg(windows)]
#[allow(unused_imports)]
pub(crate) use validate::validate_windows_wide_component;
#[allow(unused_imports)]

#[cfg(test)]
mod tests;
