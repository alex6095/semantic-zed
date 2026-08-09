use gpui::{App, BoxShadow, Hsla, px};
use ui::prelude::*;

/// Shared visual tokens for the scientific workspace surfaces.
///
/// These are deliberately opaque and derive entirely from the active Zed
/// theme. `Semantic Light` supplies the product's default surface hierarchy,
/// but users retain the ability to choose another theme without leaving the
/// Overleaf and PDF workspaces visually disconnected from the rest of Zed.
#[derive(Clone)]
pub(crate) struct ScientificPalette {
    pub sidebar: Hsla,
    pub canvas: Hsla,
    pub card: Hsla,
    pub pdf_surround: Hsla,
    pub divider: Hsla,
    pub accent_tint: Hsla,
    pub card_shadow: Vec<BoxShadow>,
}

impl ScientificPalette {
    pub fn resolve(cx: &App) -> Self {
        let colors = cx.theme().colors();
        let is_light = colors.editor_background.l > 0.55;

        let mut close_shadow = colors.text;
        close_shadow.a = if is_light { 0.045 } else { 0.16 };
        let mut ambient_shadow = colors.text;
        ambient_shadow.a = if is_light { 0.032 } else { 0.10 };

        Self {
            sidebar: colors.panel_background,
            canvas: colors.editor_background,
            card: colors.elevated_surface_background,
            pdf_surround: colors.background,
            divider: colors.border_variant,
            accent_tint: colors.ghost_element_selected,
            card_shadow: vec![
                BoxShadow::new(px(0.0), px(1.0), close_shadow).blur_radius(px(2.0)),
                BoxShadow::new(px(0.0), px(4.0), ambient_shadow).blur_radius(px(12.0)),
            ],
        }
    }
}
