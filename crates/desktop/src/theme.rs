//! 设计令牌。
//!
//! **移植自 orbit（`crates/orbit-pi/src/theme/tokens.rs`），
//! 而 orbit 的注释写明它是一对一移植自 Zed 的 `ui` crate**
//! （`styles/{units,spacing,typography,elevation,animation}.rs`）。
//!
//! 换句话说这是 Zed 的尺寸体系，经由 orbit 传到我们这里。
//! 只取了本应用需要的部分，未整份复制。
//!
//! 配色取了 GitHub 深色（与 harness 现有界面一致），
//! 尺寸与层级沿用 Zed 的语义。
//!
//! 令牌是全刻度的：界面逐块铺开时会陆续用上，暂未读到的值
//! 是预期而非死代码（orbit 的 tokens.rs 也是这么处理的）。

#![allow(dead_code)]

use gpui::{px, Pixels};

// ── 尺寸单位（Zed `styles/units.rs`）──────────────────────────

/// Zed 的 rem，默认 UI 字号下的像素值。
pub const BASE_REM_PX: f32 = 16.0;

/// 默认 UI 字号。
pub const DEFAULT_UI_FONT_SIZE: f32 = 14.0;

/// rem 转像素。
pub fn rems(value: f32) -> Pixels {
    px(value * BASE_REM_PX)
}

// ── 间距（Zed `styles/spacing.rs` 的 DynamicSpacing）─────────

/// 动态间距。数值对齐 Zed 的 DynamicSpacing 刻度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    Base04,
    Base06,
    Base08,
    Base10,
    Base12,
    Base16,
    Base20,
    Base24,
    Base32,
}

impl Space {
    pub fn px(self) -> Pixels {
        px(match self {
            Self::Base04 => 4.0,
            Self::Base06 => 6.0,
            Self::Base08 => 8.0,
            Self::Base10 => 10.0,
            Self::Base12 => 12.0,
            Self::Base16 => 16.0,
            Self::Base20 => 20.0,
            Self::Base24 => 24.0,
            Self::Base32 => 32.0,
        })
    }
}

// ── 字号（Zed `styles/typography.rs` 的 TextSize）────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    /// 11px，辅助文字
    XSmall,
    /// 12px，标签
    Small,
    /// 14px，正文
    Base,
    /// 16px，标题
    Large,
    /// 20px，大标题
    XLarge,
}

impl Text {
    pub fn px(self) -> Pixels {
        px(match self {
            Self::XSmall => 11.0,
            Self::Small => 12.0,
            Self::Base => 14.0,
            Self::Large => 16.0,
            Self::XLarge => 20.0,
        })
    }
}

// ── 圆角（Zed `styles/units.rs` 的 CornerRadius）─────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Radius {
    None,
    Small,
    Medium,
    Large,
}

impl Radius {
    pub fn px(self) -> Pixels {
        px(match self {
            Self::None => 0.0,
            Self::Small => 3.0,
            Self::Medium => 6.0,
            Self::Large => 8.0,
        })
    }
}

// ── 语义色（GitHub 深色）────────────────────────────────────

pub mod color {
    use gpui::{rgb, Hsla};

    /// 应用背景
    pub fn bg() -> Hsla {
        rgb(0x0d1117).into()
    }
    /// 面板背景
    pub fn panel() -> Hsla {
        rgb(0x161b22).into()
    }
    /// 边框
    pub fn border() -> Hsla {
        rgb(0x30363d).into()
    }
    /// 主文字
    pub fn text() -> Hsla {
        rgb(0xe6edf3).into()
    }
    /// 次要文字
    pub fn muted() -> Hsla {
        rgb(0x8b949e).into()
    }
    /// 强调（选中态、链接）
    pub fn accent() -> Hsla {
        rgb(0x2f81f7).into()
    }
    /// 成功
    pub fn ok() -> Hsla {
        rgb(0x3fb950).into()
    }
    /// 警告
    pub fn warn() -> Hsla {
        rgb(0xd29922).into()
    }
    /// 错误
    pub fn err() -> Hsla {
        rgb(0xf85149).into()
    }
    /// 代码/等宽背景
    pub fn code_bg() -> Hsla {
        rgb(0x0d1117).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_matches_zed_scale() {
        // Zed 的 DynamicSpacing 刻度
        assert_eq!(Space::Base04.px(), px(4.0));
        assert_eq!(Space::Base08.px(), px(8.0));
        assert_eq!(Space::Base16.px(), px(16.0));
        assert_eq!(Space::Base32.px(), px(32.0));
    }

    #[test]
    fn text_sizes_ordered() {
        assert!(Text::XSmall.px() < Text::Small.px());
        assert!(Text::Small.px() < Text::Base.px());
        assert!(Text::Base.px() < Text::Large.px());
        assert!(Text::Large.px() < Text::XLarge.px());
    }

    #[test]
    fn radius_ordered() {
        assert!(Radius::None.px() < Radius::Small.px());
        assert!(Radius::Small.px() < Radius::Medium.px());
        assert!(Radius::Medium.px() < Radius::Large.px());
    }

    #[test]
    fn rems_follow_base() {
        assert_eq!(rems(1.0), px(16.0));
        assert_eq!(rems(0.5), px(8.0));
    }
}
