use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Image as GpuiImage, ImageFormat as GpuiImageFormat, Render, Task, WeakEntity, Window, img,
    prelude::*,
};
use image::ImageFormat;
use pdfium_render::prelude::*;
use std::{
    env,
    io::Cursor,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};
use ui::{Button, ButtonStyle, Color, Icon, IconName, IconSize, Label, LabelSize, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::ToggleOverleafPdf;
use crate::design::ScientificPalette;

const PDF_PANEL_KEY: &str = "SemanticZedPdfPreviewPanel";
const PDF_CACHE_PATH: &str = ".semantic-zed/output/output.pdf";
const PDF_RENDER_WIDTH: i32 = 1800;
const PDF_RENDER_MAX_HEIGHT: i32 = 2800;

// `pdfium-render` owns the dynamic-library bindings globally. Keep our own
// small initialization gate as well, so a page reload or navigation does not
// attempt to bind a second copy of PDFium.
static PDFIUM_INITIALIZATION: OnceLock<()> = OnceLock::new();
static PDFIUM_INITIALIZATION_LOCK: Mutex<()> = Mutex::new(());

/// A cross-platform native PDF preview panel.
///
/// The UI is entirely GPUI. Page rasterization goes through PDFium, loaded
/// from the application bundle rather than through a browser webview or an
/// operating-system PDF app. The macOS, Windows, and Linux packagers place the
/// appropriate Pdfium shared library in the same `Resources/pdfium` location.
pub struct PdfPreviewPanel {
    focus_handle: FocusHandle,
    pdf_path: Option<PathBuf>,
    page_index: usize,
    page_count: usize,
    rendered_page: Option<Arc<GpuiImage>>,
    message: String,
    loading: bool,
    generation: u64,
    task: Task<()>,
}

impl PdfPreviewPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| Self {
                focus_handle: cx.focus_handle(),
                pdf_path: None,
                page_index: 0,
                page_count: 0,
                rendered_page: None,
                message: "Compile an Overleaf project, then open its PDF preview.".to_string(),
                loading: false,
                generation: 0,
                task: Task::ready(()),
            })
        })
    }

    pub fn open_for_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let pdf_path = root.join(PDF_CACHE_PATH);
        let page_index = if self.pdf_path.as_ref() == Some(&pdf_path) {
            self.page_index
        } else {
            0
        };
        self.pdf_path = Some(pdf_path);
        self.load_page(page_index, window, cx);
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_page(self.page_index, window, cx);
    }

    fn previous_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page_index > 0 {
            self.load_page(self.page_index - 1, window, cx);
        }
    }

    fn next_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page_index + 1 < self.page_count {
            self.load_page(self.page_index + 1, window, cx);
        }
    }

    fn open_externally(&mut self) {
        let Some(path) = self.pdf_path.as_ref() else {
            return;
        };
        let _ = open_with_system_viewer(path);
    }

    fn load_page(&mut self, page_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pdf_path) = self.pdf_path.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.message = if pdf_path.is_file() {
            "Rendering PDF…".to_string()
        } else {
            "No compiled PDF yet. Choose Compile in Overleaf first.".to_string()
        };
        cx.notify();

        let render_task = cx.background_spawn(async move { render_page(&pdf_path, page_index) });
        self.task = cx.spawn_in(window, async move |this, cx| {
            let result = render_task.await;
            this.update_in(cx, |this, _, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(page) => {
                        this.page_index = page.page_index;
                        this.page_count = page.page_count;
                        this.rendered_page = Some(Arc::new(GpuiImage::from_bytes(
                            GpuiImageFormat::Png,
                            page.png,
                        )));
                        this.message =
                            format!("Page {} of {}", page.page_index + 1, page.page_count);
                    }
                    Err(error) => {
                        this.rendered_page = None;
                        this.page_count = 0;
                        this.message = preview_error_message(&error);
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }
}

impl Focusable for PdfPreviewPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PdfPreviewPanel {}

impl Render for PdfPreviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = ScientificPalette::resolve(cx);
        let can_go_back = !self.loading && self.page_index > 0;
        let can_go_forward = !self.loading && self.page_index + 1 < self.page_count;
        let has_pdf = self.pdf_path.is_some();
        let page = self.rendered_page.clone();
        let message = self.message.clone();

        v_flex()
            .id("semantic-zed-pdf-preview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.canvas)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .bg(palette.card)
                    .border_b_1()
                    .border_color(palette.divider)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .w_6()
                                    .h_6()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .bg(palette.accent_tint)
                                    .child(
                                        Icon::new(IconName::FileDoc)
                                            .size(IconSize::Small)
                                            .color(Color::Accent),
                                    ),
                            )
                            .child(
                                Label::new("PDF Preview")
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("semantic-zed-pdf-open-system", "Open")
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!has_pdf)
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.open_externally();
                                    })),
                            )
                            .child(
                                Button::new("semantic-zed-pdf-reload", "Reload")
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!has_pdf || self.loading)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reload(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .bg(palette.card)
                    .border_b_1()
                    .border_color(palette.divider)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("semantic-zed-pdf-previous", "‹")
                                    .style(ButtonStyle::OutlinedGhost)
                                    .disabled(!can_go_back)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.previous_page(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("semantic-zed-pdf-next", "›")
                                    .style(ButtonStyle::OutlinedGhost)
                                    .disabled(!can_go_forward)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.next_page(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        Label::new(message)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                div()
                    .id("semantic-zed-pdf-page-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .bg(palette.pdf_surround)
                    .when_some(page, |this, page| {
                        this.child(
                            div().w_full().items_center().child(
                                div()
                                    .bg(palette.card)
                                    .rounded_lg()
                                    .overflow_hidden()
                                    .shadow(palette.card_shadow.clone())
                                    .child(
                                        img(page).id("semantic-zed-pdf-page").max_w_full().h_auto(),
                                    ),
                            ),
                        )
                    })
                    .when(self.rendered_page.is_none(), |this| {
                        this.child(
                            v_flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::FileDoc)
                                        .size(IconSize::XLarge)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("Compiled Overleaf PDF")
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::MEDIUM),
                                )
                                .child(
                                    Label::new(self.message.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    }),
            )
    }
}

impl Panel for PdfPreviewPanel {
    fn persistent_name() -> &'static str {
        "Semantic Zed PDF Preview"
    }

    fn panel_key() -> &'static str {
        PDF_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> gpui::Pixels {
        gpui::px(520.0)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::FileDoc)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("PDF Preview")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleOverleafPdf)
    }

    fn activation_focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        20
    }
}

struct RenderedPdfPage {
    page_index: usize,
    page_count: usize,
    png: Vec<u8>,
}

fn render_page(path: &Path, page_index: usize) -> Result<RenderedPdfPage> {
    if !path.is_file() {
        bail!("No compiled PDF exists yet.");
    }
    let pdfium_path = bundled_pdfium_library()?;
    let pdfium = bundled_pdfium(&pdfium_path)?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let page_count = document.pages().len();
    if page_count <= 0 {
        bail!("The compiled PDF contains no pages.");
    }
    let page_count = page_count as usize;
    if page_index >= page_count {
        bail!(
            "Page {} is outside this {}-page PDF.",
            page_index + 1,
            page_count
        );
    }
    let page = document
        .pages()
        .get(page_index as i32)
        .context("loading PDF page")?;
    let bitmap = page
        .render_with_config(
            &PdfRenderConfig::new()
                .set_target_width(PDF_RENDER_WIDTH)
                .set_maximum_height(PDF_RENDER_MAX_HEIGHT),
        )
        .context("rendering PDF page")?;
    let image = bitmap
        .as_image()
        .context("converting PDF page to an image")?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .context("encoding PDF page")?;

    Ok(RenderedPdfPage {
        page_index,
        page_count,
        png,
    })
}

fn bundled_pdfium(path: &Path) -> Result<Pdfium> {
    let initialization_guard = PDFIUM_INITIALIZATION_LOCK
        .lock()
        .map_err(|_| anyhow!("PDF preview initialization lock was poisoned."))?;

    if PDFIUM_INITIALIZATION.get().is_none() {
        match Pdfium::bind_to_library(path) {
            Ok(bindings) => {
                // Constructing this short-lived wrapper initializes PDFium and
                // stores the bindings inside `pdfium-render`'s process-global
                // binding cell. Subsequent wrappers safely reuse that cell.
                let _pdfium = Pdfium::new(bindings);
            }
            Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => {
                // Another native PDF feature initialized the same renderer
                // before this panel did. Reuse its thread-safe binding.
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("loading PDF renderer from {}", path.display()));
            }
        }
        PDFIUM_INITIALIZATION
            .set(())
            .map_err(|_| anyhow!("PDF preview renderer initialized concurrently."))?;
    }

    drop(initialization_guard);
    Ok(Pdfium::default())
}

fn bundled_pdfium_library() -> Result<PathBuf> {
    let library_name = pdfium_library_name()?;
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("SEMANTIC_ZED_PDFIUM_LIB") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(resources) = executable
            .parent()
            .and_then(Path::parent)
            .map(|contents| contents.join("Resources"))
        {
            candidates.push(resources.join("pdfium").join(library_name));
        }
        if let Some(executable_dir) = executable.parent() {
            candidates.push(
                executable_dir
                    .join("resources")
                    .join("pdfium")
                    .join(library_name),
            );
            candidates.push(
                executable_dir
                    .join("Resources")
                    .join("pdfium")
                    .join(library_name),
            );
        }
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            anyhow!(
                "This Semantic Zed build has no bundled PDF renderer. Rebuild the app so its PDFium resource is included."
            )
        })
}

#[cfg(target_os = "macos")]
fn pdfium_library_name() -> Result<&'static str> {
    Ok("libpdfium.dylib")
}

#[cfg(target_os = "windows")]
fn pdfium_library_name() -> Result<&'static str> {
    Ok("pdfium.dll")
}

#[cfg(target_os = "linux")]
fn pdfium_library_name() -> Result<&'static str> {
    Ok("libpdfium.so")
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn pdfium_library_name() -> Result<&'static str> {
    bail!("PDF preview is unsupported on this operating system.")
}

// The native preview remains primary. This explicit user-invoked fallback must launch the
// platform viewer and is already called from a GPUI background task.
#[allow(clippy::disallowed_methods)]
fn open_with_system_viewer(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let mut command = return Err(anyhow!(
        "No system PDF viewer is configured for this platform."
    ));

    command
        .arg(path)
        .spawn()
        .context("opening PDF externally")?;
    Ok(())
}

fn preview_error_message(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    if message.contains("no bundled PDF renderer") {
        return "PDF preview is not packaged in this build yet.".to_string();
    }
    if message.contains("No compiled PDF") {
        return "No compiled PDF yet. Choose Compile in Overleaf first.".to_string();
    }
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("PDF preview could not be rendered.")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_current_platform_to_a_pdfium_library_name() {
        assert!(
            !pdfium_library_name()
                .expect("supported desktop target")
                .is_empty()
        );
    }

    #[test]
    fn presents_a_concise_missing_renderer_message() {
        let error = anyhow!("This Semantic Zed build has no bundled PDF renderer.");
        assert_eq!(
            preview_error_message(&error),
            "PDF preview is not packaged in this build yet."
        );
    }

    #[test]
    #[ignore = "requires the packaging-time PDFium resource"]
    fn renders_the_existing_cross_platform_pdf_fixture() {
        let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(4)
            .expect("Semantic Zed repository root")
            .to_path_buf();
        let fixture =
            repository_root.join("views/pdf-viewer/vendor/web/compressed.tracemonkey-pldi-09.pdf");
        let page = render_page(&fixture, 0).expect("render PDF fixture");
        assert!(page.page_count > 0);
        assert!(page.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let rerendered_page = render_page(&fixture, 0).expect("render PDF fixture twice");
        assert!(rerendered_page.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
