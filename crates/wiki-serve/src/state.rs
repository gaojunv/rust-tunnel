// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! 应用状态：vault 根目录的可变持有。

use std::path::PathBuf;
use std::sync::Mutex;

/// 应用全局状态（vault 根目录）。
#[derive(Debug)]
pub struct AppState {
    vault_root: Mutex<PathBuf>,
}

impl AppState {
    /// 以指定 `root` 构造状态。
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            vault_root: Mutex::new(root),
        }
    }

    /// 从环境变量 `WIKI_VAULT_ROOT` 或默认值构造。
    ///
    /// 优先级：`WIKI_VAULT_ROOT` → `$HOME/wiki-vault` → `./vault`。
    #[must_use]
    pub fn from_env_or_default() -> Self {
        let root = std::env::var_os("WIKI_VAULT_ROOT").map(PathBuf::from);
        let root = root.map_or_else(
            || {
                std::env::var_os("HOME").map_or_else(
                    || PathBuf::from("./vault"),
                    |h| PathBuf::from(h).join("wiki-vault"),
                )
            },
            PathBuf::from,
        );
        Self::new(root)
    }

    /// 克隆返回当前 vault 根目录（锁不跨调用持有）。
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.vault_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 更新 vault 根目录。
    pub fn set_root(&self, next: PathBuf) {
        let mut guard = self
            .vault_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = next;
    }
}
