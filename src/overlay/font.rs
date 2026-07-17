//! 字体光栅化的 cosmic-text 封装：字节级 lazy + 线程内 FontSystem / SwashCache 池
//!
//! `FontSystem` 不是 Sync（cosmic-text 设计）；`SwashCache` 带内部 LRU cache。
//! 单线程多次调用 `rasterize_text` 时复用同一 `FontSystem` / `SwashCache`，
//! 避免每次都重付 ~30-50ms 冷启动（OTF 解析 + swash backend 初始化）。
//!
//! 字节级缓存（`REGULAR_BYTES` / `BOLD_BYTES`）跨线程共享；FontSystem 池只在线程内。

use std::cell::RefCell;

use cosmic_text::{FontSystem, SwashCache};

use crate::overlay::drawing::FontWeight;

/// 跨线程共享的 OTF 字节缓存（lazy + 全局唯一）
static REGULAR_BYTES: once_cell::sync::Lazy<Vec<u8>> =
    once_cell::sync::Lazy::new(|| FontWeight::Normal.font_bytes().to_vec());

static BOLD_BYTES: once_cell::sync::Lazy<Vec<u8>> =
    once_cell::sync::Lazy::new(|| FontWeight::Bold.font_bytes().to_vec());

thread_local! {
    static FONT_SYSTEM: RefCell<Option<FontSystem>> = RefCell::new(None);
    static SWASH_CACHE: RefCell<Option<SwashCache>> = RefCell::new(None);
}

/// 当前线程懒初始化并借用 FontSystem（Regular + Bold 已 load）
pub fn with_font_system<R>(f: impl FnOnce(&mut FontSystem) -> R) -> R {
    FONT_SYSTEM.with(|cell| {
        let mut b = cell.borrow_mut();
        if b.is_none() {
            let mut fs = FontSystem::new();
            fs.db_mut().load_font_data(REGULAR_BYTES.clone());
            fs.db_mut().load_font_data(BOLD_BYTES.clone());
            *b = Some(fs);
        }
        f(b.as_mut().unwrap())
    })
}

/// 当前线程懒初始化并借用 SwashCache
pub fn with_swash_cache<R>(f: impl FnOnce(&mut SwashCache) -> R) -> R {
    SWASH_CACHE.with(|cell| {
        let mut b = cell.borrow_mut();
        if b.is_none() {
            *b = Some(SwashCache::new());
        }
        f(b.as_mut().unwrap())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_system_loads_at_least_two_families_after_first_call() {
        let count = with_font_system(|fs| fs.db().len());
        assert!(count >= 2, "FontDatabase 应至少含 2 份字体: actual={}", count);
    }

    #[test]
    fn swash_cache_initializes_without_panic() {
        let _ = with_swash_cache(|_| ());
    }
}
