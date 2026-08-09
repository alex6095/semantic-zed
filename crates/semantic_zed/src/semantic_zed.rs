use anyhow::{Context as _, Result, anyhow, bail};
use editor::{
    Editor, EditorElement, EditorStyle, NavigationOverlayKey, NavigationOverlayLabel,
    NavigationTargetOverlay,
};
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    PathPromptOptions, Styled, Subscription, Task, TextStyle, WeakEntity, Window, actions,
    prelude::*,
};
use language::{Bias, Point};
use project::{DirectoryLister, Project};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use settings::Settings as _;
use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use theme_settings::ThemeSettings;
use ui::{
    Button, ButtonStyle, Color, Icon, IconName, IconSize, Label, LabelSize, ListItem,
    ListItemSpacing, TintColor, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    OpenMode, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

mod pdf_preview;

pub use pdf_preview::PdfPreviewPanel;

const PAPER_PANEL_KEY: &str = "SemanticZedPaperPanel";
const PROJECT_METADATA_PATH: &str = ".semantic-zed/project.json";
const RUNTIME_METADATA_PATH: &str = ".semantic-zed/runtime.json";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const PRESENCE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const OVERLEAF_SERVER: &str = "https://www.overleaf.com/";

enum OverleafPresenceOverlay {}

const OVERLEAF_PRESENCE_OVERLAY_KEY: NavigationOverlayKey =
    NavigationOverlayKey::unique::<OverleafPresenceOverlay>();

actions!(
    semantic_zed,
    [
        /// Opens the Overleaf panel for an initialized local replica.
        ToggleOverleaf,
        /// Opens a secure browser sign-in flow for the local Overleaf runtime.
        LoginToOverleaf,
        /// Refreshes the Overleaf integration status from its local sync daemon.
        RefreshOverleaf,
        /// Starts the configured Semantic Zed Overleaf sync daemon.
        StartOverleafSync,
        /// Runs the synchronized Overleaf Cloud compile barrier.
        CompileOverleaf,
        /// Opens the native PDF preview for the current Overleaf project's latest Cloud build.
        ToggleOverleafPdf,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleOverleaf, window, cx| {
            workspace.toggle_panel_focus::<PaperPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &LoginToOverleaf, window, cx| {
            if let Some(panel) = workspace.panel::<PaperPanel>(cx) {
                panel.update(cx, |panel, cx| panel.login_to_overleaf(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &RefreshOverleaf, window, cx| {
            let paper_root = PaperPanel::paper_root_for_workspace(workspace, cx);
            if let Some(panel) = workspace.panel::<PaperPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.refresh_with_root(paper_root, window, cx)
                });
            }
        });
        workspace.register_action(|workspace, _: &StartOverleafSync, window, cx| {
            if let Some(panel) = workspace.panel::<PaperPanel>(cx) {
                panel.update(cx, |panel, cx| panel.start_sync(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &CompileOverleaf, window, cx| {
            if let Some(panel) = workspace.panel::<PaperPanel>(cx) {
                panel.update(cx, |panel, cx| panel.compile(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &ToggleOverleafPdf, window, cx| {
            let root = PaperPanel::paper_root_for_workspace(workspace, cx);
            let Some(root) = root else {
                return;
            };
            workspace.reveal_panel::<PdfPreviewPanel>(window, cx);
            if let Some(panel) = workspace.panel::<PdfPreviewPanel>(cx) {
                panel.update(cx, |panel, cx| panel.open_for_root(root, window, cx));
            }
        });
    })
    .detach();
}

pub struct PaperPanel {
    focus_handle: FocusHandle,
    paper_root: Option<PathBuf>,
    status: PaperStatus,
    login: LoginState,
    task: Task<()>,
    login_task: Task<()>,
    projects_task: Task<()>,
    presence_task: Task<()>,
    workspace: WeakEntity<Workspace>,
    projects: ProjectListState,
    initializing_project: Option<String>,
    project_message: Option<String>,
    new_project_editor: Entity<Editor>,
    show_new_project_form: bool,
    creating_project: bool,
    presence: Vec<OverleafPresence>,
    subscriptions: Vec<Subscription>,
}

impl PaperPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let workspace_handle = workspace.clone();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace_handle, workspace, window, cx)
        })
    }

    fn new(
        workspace_handle: WeakEntity<Workspace>,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_entity = cx.entity();
        let new_project_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Project name", window, cx);
            editor
        });
        let panel = cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            paper_root: None,
            status: PaperStatus::Unavailable,
            login: LoginState::Checking,
            task: Task::ready(()),
            login_task: Task::ready(()),
            projects_task: Task::ready(()),
            presence_task: Task::ready(()),
            workspace: workspace_handle,
            projects: ProjectListState::NotConnected,
            initializing_project: None,
            project_message: None,
            new_project_editor,
            show_new_project_form: false,
            creating_project: false,
            presence: Vec::new(),
            subscriptions: Vec::new(),
        });

        panel.update(cx, |panel, cx| {
            panel.subscribe_to_project(&project, window, cx);
            panel.subscribe_to_workspace(&workspace_entity, window, cx);
            panel.refresh_from_project(&project, window, cx);
            panel.refresh_login_from_credentials(window, cx);
        });
        panel
    }

    pub fn has_paper_root(workspace: &Workspace, cx: &App) -> bool {
        Self::paper_root_for_workspace(workspace, cx).is_some()
    }

    fn paper_root_for_workspace(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
        let project = workspace.project();
        Self::paper_root_for_project(project, cx)
    }

    fn paper_root_for_project(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
        project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .find(|root| root.join(PROJECT_METADATA_PATH).is_file())
    }

    fn refresh_with_root(
        &mut self,
        paper_root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paper_root = paper_root;
        self.refresh_current_root(window, cx);
    }

    fn refresh_from_project(
        &mut self,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paper_root = Self::paper_root_for_project(project, cx);
        if paper_root != self.paper_root {
            self.paper_root = paper_root;
            self.refresh_current_root(window, cx);
        }
    }

    fn subscribe_to_project(
        &mut self,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscriptions.push(cx.subscribe_in(
            project,
            window,
            |this, project, event: &project::Event, window, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_)
                        | project::Event::WorktreeRemoved(_)
                        | project::Event::WorktreeOrderChanged
                        | project::Event::WorktreePathsChanged { .. }
                ) {
                    this.refresh_from_project(project, window, cx);
                }
            },
        ));
    }

    fn subscribe_to_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscriptions.push(cx.subscribe_in(
            workspace,
            window,
            |this, _, event: &workspace::Event, window, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    this.schedule_presence_markers(window, cx);
                }
            },
        ));
    }

    fn refresh_current_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            self.status = PaperStatus::Unavailable;
            self.clear_presence(window, cx);
            cx.notify();
            return;
        };

        self.status = PaperStatus::Loading;
        cx.notify();
        let status_task = cx.background_spawn(async move { query_status(&root) });
        self.task = cx.spawn_in(window, async move |this, cx| {
            let status = status_task.await;
            this.update_in(cx, |this, window, cx| {
                let should_stream = matches!(&status, Ok(status) if status.is_live());
                this.status = PaperStatus::from_result(status);
                if should_stream {
                    this.restart_presence_stream(window, cx);
                } else {
                    this.clear_presence(window, cx);
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn start_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            return;
        };
        self.status = PaperStatus::Loading;
        cx.notify();
        let start_task = cx.background_spawn(async move { start_sync_and_query_status(&root) });
        self.task = cx.spawn_in(window, async move |this, cx| {
            let status = start_task.await;
            this.update_in(cx, |this, window, cx| {
                let should_stream = matches!(&status, Ok(status) if status.is_live());
                this.status = PaperStatus::from_result(status);
                if should_stream {
                    this.restart_presence_stream(window, cx);
                } else {
                    this.clear_presence(window, cx);
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn login_to_overleaf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.login.is_in_progress() {
            return;
        }

        self.login = LoginState::OpeningBrowser;
        cx.notify();
        let login_task = cx.background_spawn(async move { login_with_browser() });
        self.login_task = cx.spawn_in(window, async move |this, cx| {
            let login = login_task.await;
            this.update_in(cx, |this, window, cx| {
                this.login = LoginState::from_result(login);
                if matches!(&this.login, LoginState::Connected) {
                    this.refresh_projects(window, cx);
                }
                if matches!(&this.login, LoginState::Connected) && this.paper_root.is_some() {
                    this.refresh_current_root(window, cx);
                } else {
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn refresh_login_from_credentials(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.login = LoginState::Checking;
        cx.notify();
        let status_task = cx.background_spawn(async move { saved_login_exists() });
        self.login_task = cx.spawn_in(window, async move |this, cx| {
            let status = status_task.await;
            this.update_in(cx, |this, window, cx| {
                this.login = LoginState::from_saved_status(status);
                if matches!(&this.login, LoginState::Connected) {
                    this.refresh_projects(window, cx);
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn refresh_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(&self.login, LoginState::Connected) {
            self.projects = ProjectListState::NotConnected;
            cx.notify();
            return;
        }

        self.projects = ProjectListState::Loading;
        cx.notify();
        let list_task = cx.background_spawn(async move { list_remote_projects() });
        self.projects_task = cx.spawn_in(window, async move |this, cx| {
            let projects = list_task.await;
            this.update_in(cx, |this, _, cx| {
                this.projects = ProjectListState::from_result(projects);
                cx.notify();
            })
            .log_err();
        });
    }

    fn choose_local_replica(
        &mut self,
        remote_project: RemoteProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.initializing_project.is_some() || remote_project.is_read_only_bucket() {
            return;
        }
        self.project_message = None;
        let Some(workspace) = self.workspace.upgrade() else {
            self.project_message =
                Some("The current workspace is no longer available.".to_string());
            cx.notify();
            return;
        };
        let chooser = workspace.update(cx, |workspace, cx| {
            workspace.prompt_for_open_path(
                PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Choose a local folder".into()),
                },
                DirectoryLister::Local(
                    workspace.project().clone(),
                    workspace.app_state().fs.clone(),
                ),
                window,
                cx,
            )
        });
        self.initializing_project = Some(remote_project.name.clone());
        cx.notify();
        let workspace = self.workspace.clone();
        let project_name = remote_project.name.clone();
        let project_id = remote_project.id.clone();
        self.projects_task = cx.spawn_in(window, async move |this, cx| {
            let Some(paths) = chooser.await.log_err().flatten() else {
                this.update_in(cx, |this, _, cx| {
                    this.initializing_project = None;
                    cx.notify();
                })
                .log_err();
                return;
            };
            let Some(root) = paths.into_iter().next() else {
                this.update_in(cx, |this, _, cx| {
                    this.initializing_project = None;
                    cx.notify();
                })
                .log_err();
                return;
            };

            let init_root = root.clone();
            let result = cx
                .background_spawn(async move { initialize_local_replica(&project_id, &init_root) })
                .await;
            match result {
                Ok(()) => {
                    // Bootstrap intentionally stops after it has made a
                    // durable replica marker. Start the long-lived daemon
                    // before changing windows so the newly opened workspace
                    // immediately observes a live session instead of making
                    // the user press Start sync a second time.
                    let sync_root = root.clone();
                    let initial_sync = cx
                        .background_spawn(async move { start_sync_and_query_status(&sync_root) })
                        .await;
                    let open_result: Result<()> = async {
                        let workspace = workspace
                            .upgrade()
                            .ok_or_else(|| anyhow!("The current workspace was closed."))?;
                        let open_workspace = workspace.update_in(cx, |workspace, window, cx| {
                            workspace.open_workspace_for_paths(
                                OpenMode::Activate,
                                vec![root.clone()],
                                window,
                                cx,
                            )
                        })?;
                        open_workspace.await?;
                        Ok(())
                    }
                    .await;
                    match open_result {
                        Ok(()) => {
                            this.update_in(cx, |this, _window, cx| {
                                this.initializing_project = None;
                                this.paper_root = Some(root);
                                this.status = PaperStatus::from_result(initial_sync);
                                cx.notify();
                            })
                            .log_err();
                        }
                        Err(error) => {
                            this.update_in(cx, |this, _, cx| {
                                this.initializing_project = None;
                                this.project_message = Some(format!(
                                    "The replica was created but could not be opened: {}",
                                    local_replica_error_message(&error)
                                ));
                                cx.notify();
                            })
                            .log_err();
                        }
                    }
                }
                Err(error) => {
                    this.update_in(cx, |this, _, cx| {
                        this.initializing_project = None;
                        this.project_message = Some(format!(
                            "Could not connect {project_name}: {}",
                            local_replica_error_message(&error)
                        ));
                        cx.notify();
                    })
                    .log_err();
                }
            }
        });
    }

    fn show_new_project_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(&self.login, LoginState::Connected) {
            return;
        }
        self.project_message = None;
        self.show_new_project_form = true;
        window.focus(&self.new_project_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn cancel_new_project_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating_project {
            return;
        }
        self.show_new_project_form = false;
        self.new_project_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        cx.notify();
    }

    fn create_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating_project || !matches!(&self.login, LoginState::Connected) {
            return;
        }
        let name = self.new_project_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            self.project_message = Some("Enter a project name before creating it.".to_string());
            cx.notify();
            return;
        }

        self.creating_project = true;
        self.project_message = None;
        cx.notify();
        let create_task = cx.background_spawn({
            let name = name.clone();
            async move { create_remote_project(&name) }
        });
        self.projects_task = cx.spawn_in(window, async move |this, cx| {
            let result = create_task.await;
            this.update_in(cx, |this, window, cx| {
                this.creating_project = false;
                match result {
                    Ok(()) => {
                        this.show_new_project_form = false;
                        this.new_project_editor
                            .update(cx, |editor, cx| editor.set_text("", window, cx));
                        this.project_message = Some(format!(
                            "Created {name}. Choose it to make a local replica."
                        ));
                        this.refresh_projects(window, cx);
                    }
                    Err(error) => {
                        this.project_message = Some(format!(
                            "Could not create {name}: {}",
                            local_replica_error_message(&error)
                        ));
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn compile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            return;
        };
        self.status = PaperStatus::Compiling;
        cx.notify();
        let compile_task = cx.background_spawn(async move { compile_and_query_status(&root) });
        self.task = cx.spawn_in(window, async move |this, cx| {
            let status = compile_task.await;
            this.update_in(cx, |this, window, cx| {
                let compiled = status.is_ok();
                this.status = PaperStatus::from_result(status);
                if compiled {
                    this.preview_pdf(window, cx);
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn preview_pdf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            return;
        };
        let workspace = self.workspace.clone();
        // Revealing a dock panel asks the workspace to inspect all panel
        // handles, including this PaperPanel. Run after the current button
        // callback releases its GPUI entity lease to avoid a double-read panic.
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                workspace.reveal_panel::<PdfPreviewPanel>(window, cx);
                if let Some(panel) = workspace.panel::<PdfPreviewPanel>(cx) {
                    panel.update(cx, |panel, cx| panel.open_for_root(root, window, cx));
                }
            });
        });
    }

    fn clear_presence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presence_task = Task::ready(());
        if !self.presence.is_empty() {
            self.presence.clear();
            self.schedule_presence_markers(window, cx);
        }
    }

    fn restart_presence_stream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            self.clear_presence(window, cx);
            return;
        };
        let (sender, receiver) = async_channel::bounded(32);
        let stream_root = root.clone();
        let stream_task = cx.background_spawn(async move { stream_presence(&stream_root, sender) });
        self.presence_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(presence) = receiver.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this.paper_root.as_ref() != Some(&root) || this.presence == presence {
                            return;
                        }
                        this.presence = presence;
                        this.schedule_presence_markers(window, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            stream_task.await.log_err();
        });
    }

    fn schedule_presence_markers(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            return;
        };
        let workspace = self.workspace.clone();
        let presence = self.presence.clone();
        window.defer(cx, move |_window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                apply_presence_markers(workspace, &root, &presence, cx);
            });
        });
    }

    fn status_text(&self) -> String {
        self.status.summary()
    }

    fn render_new_project_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let input_style = EditorStyle {
            background: cx.theme().colors().editor_background,
            local_player: cx.theme().players().local(),
            text: TextStyle {
                color: cx.theme().colors().text,
                font_family: settings.ui_font.family.clone(),
                font_features: settings.ui_font.features.clone(),
                font_fallbacks: settings.ui_font.fallbacks.clone(),
                font_size: ui::rems(0.875).into(),
                font_weight: settings.ui_font.weight,
                ..Default::default()
            },
            ..Default::default()
        };

        v_flex()
            .gap_2()
            .p_2()
            .bg(cx.theme().colors().surface_background)
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_md()
            .child(
                Label::new("New blank project")
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("Creates an Overleaf project; choose its local folder afterwards.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .h_8()
                    .min_w_0()
                    .px_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_md()
                    .child(EditorElement::new(&self.new_project_editor, input_style)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("semantic-zed-cancel-new-project", "Cancel")
                            .style(ButtonStyle::OutlinedGhost)
                            .disabled(self.creating_project)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_new_project_form(window, cx);
                            })),
                    )
                    .child(
                        Button::new(
                            "semantic-zed-create-new-project",
                            if self.creating_project {
                                "Creating…"
                            } else {
                                "Create project"
                            },
                        )
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .disabled(self.creating_project)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.create_new_project(window, cx);
                        })),
                    ),
            )
    }
}

impl Focusable for PaperPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PaperPanel {}

impl Render for PaperPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_root = self.paper_root.is_some();
        let status_text = self.status_text();
        let login_text = self.login.summary();
        let login_button_label = self.login.button_label();
        let root_text = self
            .paper_root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No initialized Overleaf replica is open.".to_string());

        let connected = matches!(&self.login, LoginState::Connected);
        let project_count = self.projects.projects().len();
        let presence_count = self.presence.len();
        let mut project_rows = v_flex().id("semantic-zed-project-list").gap_1();
        if let Some(detail) = self.projects.detail() {
            project_rows = project_rows.child(
                Label::new(detail.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        if let Some(project_name) = &self.initializing_project {
            project_rows = project_rows.child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::ArrowCircle).size(IconSize::XSmall))
                    .child(
                        Label::new(format!("Preparing {project_name}…")).size(LabelSize::XSmall),
                    ),
            );
        }
        if let Some(message) = &self.project_message {
            project_rows = project_rows.child(
                Label::new(message.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        let mut projects = self.projects.projects().to_vec();
        // Keep Overleaf's dashboard recency order within each lifecycle bucket.
        // Alphabetizing here made the native view disagree with the web app
        // and with the original project-manager extension.
        projects.sort_by_key(RemoteProject::bucket_rank);
        let mut previous_bucket = None;
        for project in projects {
            if previous_bucket != Some(project.bucket_label()) {
                project_rows = project_rows.child(
                    Label::new(project.bucket_label())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
                previous_bucket = Some(project.bucket_label());
            }
            let disabled = self.initializing_project.is_some()
                || self.creating_project
                || project.is_read_only_bucket();
            let status_label = project.access_label();
            let updated_label = project.updated_label();
            project_rows = project_rows.child(
                ListItem::new(format!("semantic-zed-project-{}", project.id))
                    .spacing(ListItemSpacing::Dense)
                    .outlined()
                    .rounded()
                    .disabled(disabled)
                    .start_slot(
                        Icon::new(if project.is_read_only_bucket() {
                            IconName::Archive
                        } else {
                            IconName::FileDoc
                        })
                        .size(IconSize::Small)
                        .color(if project.is_read_only_bucket() {
                            Color::Muted
                        } else {
                            Color::Accent
                        }),
                    )
                    .end_slot(
                        Label::new(status_label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                Label::new(project.name.clone())
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::MEDIUM)
                                    .truncate(),
                            )
                            .when_some(updated_label, |this, updated_label| {
                                this.child(
                                    Label::new(updated_label)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_local_replica(project.clone(), window, cx);
                    })),
            );
        }

        v_flex()
            .id("semantic-zed-paper-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .gap_2()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::FileDoc).color(Color::Accent))
                            .child(Label::new("Overleaf").weight(FontWeight::SEMIBOLD)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("semantic-zed-new-project", "New project")
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .disabled(!connected || self.creating_project)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_new_project_form(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("semantic-zed-project-refresh", "Refresh")
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!connected || self.creating_project)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.refresh_projects(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(cx.theme().colors().surface_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(if connected {
                                    IconName::Check
                                } else {
                                    IconName::CloudDownload
                                })
                                .size(IconSize::Small)
                                .color(if connected {
                                    Color::Success
                                } else {
                                    Color::Muted
                                }),
                            )
                            .child(
                                Label::new(if connected {
                                    "Overleaf connected"
                                } else {
                                    "Connect your Overleaf account"
                                })
                                .size(LabelSize::Small)
                                .weight(FontWeight::MEDIUM),
                            ),
                    )
                    .child(
                        Label::new(login_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("semantic-zed-login", login_button_label)
                            .style(if connected {
                                ButtonStyle::OutlinedGhost
                            } else {
                                ButtonStyle::Tinted(TintColor::Accent)
                            })
                            .full_width()
                            .disabled(self.login.is_in_progress())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.login_to_overleaf(window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("Projects")
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(format!("{project_count} projects"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .when(self.show_new_project_form, |this| {
                        this.child(self.render_new_project_form(cx))
                    })
                    .child(
                        Label::new(self.projects.summary())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .id("semantic-zed-project-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(project_rows),
            )
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(cx.theme().colors().surface_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .child(
                        Label::new("Current replica")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(status_text)
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM),
                    )
                    .child(
                        Label::new(root_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .when(presence_count > 0, |this| {
                        this.child(
                            Label::new(format!(
                                "{presence_count} Overleaf collaborator{} in the editor",
                                if presence_count == 1 { "" } else { "s" }
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("semantic-zed-start-sync", "Start sync")
                                    .style(ButtonStyle::OutlinedGhost)
                                    .disabled(!has_root)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_sync(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("semantic-zed-compile", "Compile")
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .disabled(!self.status.can_compile())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.compile(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("semantic-zed-preview-pdf", "Preview PDF")
                                    .style(ButtonStyle::OutlinedGhost)
                                    .disabled(!has_root)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.preview_pdf(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

impl Panel for PaperPanel {
    fn persistent_name() -> &'static str {
        // The project picker uses the same left dock/activity-bar slot as Files.
        // The right dock remains available for PDF, agents, reviews, and comments.
        "Semantic Zed Overleaf"
    }

    fn panel_key() -> &'static str {
        PAPER_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.0)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::FileDoc)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Overleaf")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleOverleaf)
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

#[derive(Clone, Debug)]
enum PaperStatus {
    Unavailable,
    Loading,
    Compiling,
    Ready(DaemonStatus),
    Error(String),
}

#[derive(Clone, Debug)]
enum LoginState {
    Checking,
    NotConnected,
    OpeningBrowser,
    Connected,
    Failed,
}

impl LoginState {
    fn from_result(result: Result<()>) -> Self {
        if result.is_ok() {
            Self::Connected
        } else {
            Self::Failed
        }
    }

    fn from_saved_status(result: Result<bool>) -> Self {
        match result {
            Ok(true) => Self::Connected,
            Ok(false) => Self::NotConnected,
            Err(_) => Self::Failed,
        }
    }

    fn summary(&self) -> &'static str {
        match self {
            Self::Checking => "Checking the saved Overleaf connection…",
            Self::NotConnected => {
                "Opens a protected browser window. If Arc is already running, another compatible browser is used instead of starting a second Arc instance."
            }
            Self::OpeningBrowser => {
                "The app-owned browser window is open. Finish signing in there; later reauthentication reuses its saved SSO session."
            }
            Self::Connected => {
                "Connected securely. The Overleaf session is stored in macOS Keychain."
            }
            Self::Failed => {
                "The private browser login did not complete. Retry Connect to Overleaf."
            }
        }
    }

    fn is_in_progress(&self) -> bool {
        matches!(self, Self::Checking | Self::OpeningBrowser)
    }

    fn button_label(&self) -> &'static str {
        match self {
            Self::Connected => "Reauthenticate",
            Self::Failed => "Retry Connect",
            _ => "Connect to Overleaf",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct RemoteProject {
    id: String,
    name: String,
    #[serde(default, rename = "accessLevel")]
    access_level: String,
    #[serde(default, rename = "lastUpdated")]
    last_updated: Option<String>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    trashed: bool,
}

impl RemoteProject {
    fn is_read_only_bucket(&self) -> bool {
        self.archived || self.trashed
    }

    fn bucket_label(&self) -> &'static str {
        if self.trashed {
            "Trash"
        } else if self.archived {
            "Archived"
        } else {
            "My projects"
        }
    }

    fn bucket_rank(&self) -> u8 {
        if self.trashed {
            2
        } else if self.archived {
            1
        } else {
            0
        }
    }

    fn access_label(&self) -> String {
        match self.access_level.as_str() {
            "owner" => "Owner".to_string(),
            "readWrite" => "Can edit".to_string(),
            "readOnly" => "View only".to_string(),
            _ if self.access_level.is_empty() => "".to_string(),
            value => value.to_string(),
        }
    }

    fn updated_label(&self) -> Option<String> {
        self.last_updated
            .as_deref()
            .and_then(|timestamp| timestamp.get(..10))
            .map(|date| format!("Updated {date}"))
    }
}

#[derive(Deserialize)]
struct RemoteProjectResponse {
    projects: Vec<RemoteProject>,
}

#[derive(Clone, Debug)]
enum ProjectListState {
    NotConnected,
    Loading,
    Ready(Vec<RemoteProject>),
    Error(String),
}

impl ProjectListState {
    fn from_result(result: Result<Vec<RemoteProject>>) -> Self {
        match result {
            Ok(projects) => Self::Ready(projects),
            Err(error) => Self::Error(error.to_string()),
        }
    }

    fn summary(&self) -> &'static str {
        match self {
            Self::NotConnected => "Connect an Overleaf account to browse projects.",
            Self::Loading => "Loading your Overleaf projects…",
            Self::Ready(projects) if projects.is_empty() => "No Overleaf projects found.",
            Self::Ready(_) => "Choose a project to create or open its local replica.",
            Self::Error(_) => "Could not load Overleaf projects.",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Error(error) => Some(error),
            _ => None,
        }
    }

    fn projects(&self) -> &[RemoteProject] {
        match self {
            Self::Ready(projects) => projects,
            _ => &[],
        }
    }
}

impl PaperStatus {
    fn from_result(result: Result<DaemonStatus>) -> Self {
        match result {
            Ok(status) => Self::Ready(status),
            Err(error) if is_daemon_unavailable(&error) => Self::Unavailable,
            Err(error) => Self::Error(error.to_string()),
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Unavailable => "Overleaf sync is not running.".to_string(),
            Self::Loading => "Refreshing Overleaf status…".to_string(),
            Self::Compiling => "Running the synchronized Cloud compile…".to_string(),
            Self::Ready(status) => status.summary(),
            Self::Error(error) => format!("Overleaf integration error: {error}"),
        }
    }

    fn can_compile(&self) -> bool {
        matches!(self, Self::Ready(status) if status.can_compile())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DaemonStatus {
    state: String,
    project_name: Option<String>,
    pending: usize,
    conflicts: Vec<String>,
    last_compile_status: Option<String>,
}

impl DaemonStatus {
    fn from_value(value: Value) -> Result<Self> {
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("The sync daemon returned a status without a state."))?
            .to_string();
        let pending = value
            .get("documents")
            .and_then(|documents| documents.get("pending"))
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize;
        let conflicts = value
            .get("conflicts")
            .and_then(Value::as_array)
            .map(|conflicts| {
                conflicts
                    .iter()
                    .filter_map(|conflict| {
                        conflict
                            .get("path")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let project_name = value
            .get("projectName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let last_compile_status = value
            .get("lastCompile")
            .and_then(|compile| compile.get("status"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        Ok(Self {
            state,
            project_name,
            pending,
            conflicts,
            last_compile_status,
        })
    }

    fn summary(&self) -> String {
        if self.state == "auth-expired" {
            return "Overleaf login expired.".to_string();
        }
        if !self.conflicts.is_empty() {
            return format!("Overleaf conflict · {} path(s)", self.conflicts.len());
        }
        if self.state == "offline" || self.state == "reconnecting" {
            return format!("Overleaf offline · {} pending", self.pending);
        }
        format!("Overleaf {} · {} pending", self.state, self.pending)
    }

    fn can_compile(&self) -> bool {
        self.state == "live" && self.conflicts.is_empty() && self.pending == 0
    }

    fn is_live(&self) -> bool {
        self.state == "live"
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OverleafPresence {
    client_id: String,
    name: String,
    document_path: String,
    row: u32,
    column: u32,
}

impl OverleafPresence {
    fn is_valid(&self) -> bool {
        !self.client_id.is_empty()
            && !self.document_path.is_empty()
            && !self.document_path.starts_with('/')
            && !self
                .document_path
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
    }

    fn display_name(&self) -> String {
        let name = self.name.trim().chars().take(40).collect::<String>();
        if name.is_empty() {
            "Overleaf collaborator".to_string()
        } else {
            name
        }
    }
}

#[derive(Deserialize)]
struct RuntimeDescriptor {
    host: Option<String>,
    port: u16,
    token: String,
}

#[derive(Serialize)]
struct ControlRequest<'a> {
    jsonrpc: &'static str,
    id: &'static str,
    method: &'a str,
    params: Value,
    token: &'a str,
}

#[derive(Deserialize)]
struct ControlResponse {
    id: Option<Value>,
    result: Option<Value>,
    error: Option<ControlError>,
}

#[derive(Deserialize)]
struct ControlError {
    message: String,
}

fn query_status(root: &Path) -> Result<DaemonStatus> {
    let value = control_request(root, "status", json!({}))?;
    DaemonStatus::from_value(value)
}

fn start_sync_and_query_status(root: &Path) -> Result<DaemonStatus> {
    let status = overleaf_command()
        .args(["start", "--workspace"])
        .arg(root)
        .status()
        .context("starting semantic-zed-overleaf")?;
    if !status.success() {
        bail!("The local sync command exited unsuccessfully.");
    }
    query_status(root)
}

fn login_with_browser() -> Result<()> {
    let mut command = overleaf_command();
    command.args(["login", "--server", OVERLEAF_SERVER]);
    let status = command
        .status()
        .context("opening the secure Overleaf browser login")?;
    if !status.success() {
        bail!("The Overleaf browser login did not complete.");
    }
    Ok(())
}

#[derive(Deserialize)]
struct AuthStatus {
    connected: bool,
}

fn saved_login_exists() -> Result<bool> {
    let output = overleaf_command()
        .args(["auth-status", "--server", OVERLEAF_SERVER, "--json"])
        .output()
        .context("checking the saved Overleaf connection")?;
    if !output.status.success() {
        bail!("The Overleaf connection check did not complete.");
    }
    let status: AuthStatus = serde_json::from_slice(&output.stdout)
        .context("parsing the saved Overleaf connection status")?;
    Ok(status.connected)
}

fn list_remote_projects() -> Result<Vec<RemoteProject>> {
    let output = overleaf_command()
        .args(["projects", "--server", OVERLEAF_SERVER, "--json"])
        .output()
        .context("listing Overleaf projects")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(if error.is_empty() {
            "The Overleaf project listing did not complete.".to_string()
        } else {
            error
        });
    }
    let response: RemoteProjectResponse =
        serde_json::from_slice(&output.stdout).context("parsing the Overleaf project list")?;
    Ok(response.projects)
}

fn create_remote_project(name: &str) -> Result<()> {
    let output = overleaf_command()
        .args([
            "new-project",
            "--name",
            name,
            "--template",
            "none",
            "--server",
            OVERLEAF_SERVER,
            "--json",
        ])
        .output()
        .context("creating an Overleaf project")?;
    if output.status.success() {
        return Ok(());
    }
    let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!(if error.is_empty() {
        "The Overleaf project could not be created.".to_string()
    } else {
        error
    });
}

fn initialize_local_replica(project_id: &str, root: &Path) -> Result<()> {
    let existing_config = root.join(PROJECT_METADATA_PATH);
    if existing_config.is_file() {
        let existing: Value = serde_json::from_str(
            &fs::read_to_string(&existing_config)
                .with_context(|| format!("reading {}", existing_config.display()))?,
        )
        .with_context(|| format!("parsing {}", existing_config.display()))?;
        let existing_id = existing
            .get("projectId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "The selected folder already contains invalid Semantic Zed project metadata."
                )
            })?;
        if existing_id == project_id {
            // The user intentionally chose an existing local replica. Never
            // rewrite its sync metadata or silently re-bootstrap its files.
            return Ok(());
        }
        bail!(
            "The selected folder is already linked to a different Overleaf project ({existing_id})."
        );
    }
    let output = overleaf_command()
        .args(["init", "--project-id", project_id, "--workspace"])
        .arg(root)
        .output()
        .context("creating the local Overleaf replica")?;
    if output.status.success() {
        return Ok(());
    }
    let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!(if error.is_empty() {
        "The local Overleaf replica could not be initialized.".to_string()
    } else {
        error
    });
}

/// Resolve the runtime shipped inside the app bundle before falling back to a
/// developer's shell PATH. The old PATH-only lookup could launch a stale
/// globally-installed CLI, which made the native panel report that a valid
/// Keychain login was missing even though this build had already authenticated.
fn overleaf_command() -> Command {
    if let Some(path) = env::var_os("SEMANTIC_ZED_OVERLEAF_CLI") {
        return Command::new(path);
    }

    if let Ok(executable) = env::current_exe() {
        if let Some(resources) = executable
            .parent()
            .and_then(Path::parent)
            .map(|contents| contents.join("Resources"))
        {
            let bundled_cli = resources.join("semantic-zed-overleaf-cli");
            if bundled_cli.is_file() {
                return Command::new(bundled_cli);
            }
        }
    }

    Command::new("semantic-zed-overleaf")
}

fn local_replica_error_message(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    if message.contains("Socket disconnected before acknowledgement")
        || message.contains("joinProjectResponse timed out")
    {
        return "The live Overleaf connection ended during setup. Retry once; the sync log has details if it continues."
            .to_string();
    }
    if message.contains("Target directory is not empty") {
        return "Choose an empty folder, or reopen the existing linked replica.".to_string();
    }
    if message.contains("already linked to a different Overleaf project") {
        return "That folder is already linked to another Overleaf project.".to_string();
    }
    message
        .lines()
        .find(|line| !line.trim().is_empty() && !line.contains("ExperimentalWarning"))
        .unwrap_or("The request did not complete.")
        .trim()
        .to_string()
}

fn compile_and_query_status(root: &Path) -> Result<DaemonStatus> {
    control_request(root, "compile", json!({}))?;
    query_status(root)
}

fn control_request(root: &Path, method: &str, params: Value) -> Result<Value> {
    let runtime_path = root.join(RUNTIME_METADATA_PATH);
    let runtime = read_runtime(&runtime_path)?;
    let address = control_address(&runtime)?;
    let mut stream = TcpStream::connect_timeout(&address, CONTROL_TIMEOUT)
        .with_context(|| format!("connecting to the local sync daemon for {method}"))?;
    stream
        .set_read_timeout(Some(CONTROL_TIMEOUT))
        .context("setting local sync read timeout")?;
    stream
        .set_write_timeout(Some(CONTROL_TIMEOUT))
        .context("setting local sync write timeout")?;

    let request = ControlRequest {
        jsonrpc: "2.0",
        id: "semantic-zed",
        method,
        params,
        token: &runtime.token,
    };
    serde_json::to_writer(&mut stream, &request).context("serializing local sync request")?;
    stream
        .write_all(b"\n")
        .context("sending local sync request")?;
    stream.flush().context("flushing local sync request")?;

    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .context("reading local sync response")?
            == 0
        {
            bail!("The sync daemon closed the connection before responding to {method}.");
        }
        let response: ControlResponse =
            serde_json::from_str(&line).context("parsing local sync response")?;
        if response.id.as_ref() != Some(&Value::String("semantic-zed".to_string())) {
            // Presence and document notifications share the authenticated
            // control stream. They may arrive before a compile/status RPC
            // response and must not be mistaken for an empty response.
            continue;
        }
        if let Some(error) = response.error {
            bail!("The sync daemon rejected {method}: {}", error.message);
        }
        return response
            .result
            .ok_or_else(|| anyhow!("The sync daemon returned no result for {method}."));
    }
}

fn stream_presence(
    root: &Path,
    sender: async_channel::Sender<Vec<OverleafPresence>>,
) -> Result<()> {
    let mut announced_unavailable = false;
    while !sender.is_closed() {
        match stream_presence_session(root, &sender) {
            Ok(()) => return Ok(()),
            Err(_) if sender.is_closed() => return Ok(()),
            Err(_) => {
                if !announced_unavailable {
                    if sender.send_blocking(Vec::new()).is_err() {
                        return Ok(());
                    }
                    announced_unavailable = true;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Ok(())
}

fn stream_presence_session(
    root: &Path,
    sender: &async_channel::Sender<Vec<OverleafPresence>>,
) -> Result<()> {
    let runtime = read_runtime(&root.join(RUNTIME_METADATA_PATH))?;
    let address = control_address(&runtime)?;
    let mut stream = TcpStream::connect_timeout(&address, CONTROL_TIMEOUT)
        .context("connecting to the local Overleaf presence stream")?;
    stream
        .set_read_timeout(Some(PRESENCE_READ_TIMEOUT))
        .context("setting Overleaf presence read timeout")?;
    stream
        .set_write_timeout(Some(CONTROL_TIMEOUT))
        .context("setting Overleaf presence write timeout")?;
    let request = ControlRequest {
        jsonrpc: "2.0",
        id: "semantic-zed-presence",
        method: "subscribe",
        params: json!({}),
        token: &runtime.token,
    };
    serde_json::to_writer(&mut stream, &request)
        .context("serializing Overleaf presence subscription")?;
    stream
        .write_all(b"\n")
        .context("sending Overleaf presence subscription")?;
    stream
        .flush()
        .context("flushing Overleaf presence subscription")?;

    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => bail!("The local Overleaf presence stream closed."),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("reading Overleaf presence stream"),
        }
        let message: Value = serde_json::from_str(&line)
            .context("parsing a local Overleaf presence notification")?;
        if let Some(presence) = presence_from_control_message(&message)?
            && sender.send_blocking(presence).is_err()
        {
            return Ok(());
        }
    }
}

fn presence_from_control_message(message: &Value) -> Result<Option<Vec<OverleafPresence>>> {
    let cursors = if message.get("id").and_then(Value::as_str) == Some("semantic-zed-presence") {
        if let Some(error) = message
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            bail!("The sync daemon rejected the presence subscription: {error}");
        }
        message
            .get("result")
            .and_then(|result| result.get("presence"))
            .cloned()
            .unwrap_or_else(|| json!([]))
    } else if message.get("method").and_then(Value::as_str) == Some("collaborator/changed") {
        let params = message.get("params").cloned().unwrap_or_else(|| json!([]));
        params.get("cursors").cloned().unwrap_or(params)
    } else {
        return Ok(None);
    };

    let presence: Vec<OverleafPresence> = serde_json::from_value(cursors)
        .context("parsing privacy-minimized Overleaf cursor data")?;
    Ok(Some(
        presence
            .into_iter()
            .filter(OverleafPresence::is_valid)
            .collect(),
    ))
}

fn apply_presence_markers(
    workspace: &mut Workspace,
    root: &Path,
    presence: &[OverleafPresence],
    cx: &mut Context<Workspace>,
) {
    let editors = workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>();
    for editor in editors {
        let absolute_path = editor.read(cx).active_buffer(cx).and_then(|buffer| {
            let buffer = buffer.read(cx);
            let file = buffer.file()?.as_local()?;
            Some(file.abs_path(cx).to_path_buf())
        });
        let relative_path = absolute_path.and_then(|path| {
            path.strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        });
        let cursors = presence
            .iter()
            .filter(|cursor| relative_path.as_deref() == Some(cursor.document_path.as_str()))
            .cloned()
            .collect::<Vec<_>>();

        editor.update(cx, |editor, cx| {
            if cursors.is_empty() {
                editor.clear_navigation_overlays(OVERLEAF_PRESENCE_OVERLAY_KEY, cx);
                return;
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let overlays = cursors
                .into_iter()
                .map(|cursor| {
                    let point =
                        snapshot.clip_point(Point::new(cursor.row, cursor.column), Bias::Left);
                    let anchor = snapshot.anchor_after(point);
                    let participant_index = presence_participant_index(&cursor.client_id);
                    NavigationTargetOverlay {
                        target_range: anchor.clone()..anchor,
                        label: NavigationOverlayLabel {
                            text: format!("▌ {}", cursor.display_name()).into(),
                            text_color: cx
                                .theme()
                                .players()
                                .color_for_participant(participant_index)
                                .cursor,
                            x_offset: gpui::px(-1.0),
                            scale_factor: 0.82,
                        },
                        covered_text_range: None,
                    }
                })
                .collect();
            editor.set_navigation_overlays(OVERLEAF_PRESENCE_OVERLAY_KEY, overlays, cx);
        });
    }
}

fn presence_participant_index(client_id: &str) -> u32 {
    client_id.bytes().fold(2_166_136_261, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}

fn read_runtime(runtime_path: &Path) -> Result<RuntimeDescriptor> {
    let contents = fs::read_to_string(runtime_path)
        .with_context(|| format!("reading {}", runtime_path.display()))?;
    serde_json::from_str(&contents).context("parsing the local sync runtime metadata")
}

fn control_address(runtime: &RuntimeDescriptor) -> Result<SocketAddr> {
    let host = runtime.host.as_deref().unwrap_or("127.0.0.1");
    if host != "127.0.0.1" && host != "localhost" && host != "::1" {
        bail!("The paper sync daemon is not using a loopback control address.");
    }
    format!("{host}:{}", runtime.port)
        .parse()
        .context("parsing local sync control address")
}

fn is_daemon_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::TimedOut
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};
    use tempfile::TempDir;

    #[test]
    fn reports_each_safety_relevant_connection_state() {
        let live = DaemonStatus {
            state: "live".to_string(),
            project_name: None,
            pending: 0,
            conflicts: Vec::new(),
            last_compile_status: None,
        };
        assert_eq!(live.summary(), "Overleaf live · 0 pending");
        assert!(live.can_compile());

        let conflict = DaemonStatus {
            conflicts: vec!["sections/results.tex".to_string()],
            ..live.clone()
        };
        assert_eq!(conflict.summary(), "Overleaf conflict · 1 path(s)");
        assert!(!conflict.can_compile());

        let expired = DaemonStatus {
            state: "auth-expired".to_string(),
            ..live
        };
        assert_eq!(expired.summary(), "Overleaf login expired.");
        assert!(!expired.can_compile());
    }

    #[test]
    fn discovers_only_initialized_paper_roots() {
        let temporary = TempDir::new().expect("create temporary paper root");
        assert!(!temporary.path().join(PROJECT_METADATA_PATH).is_file());
        fs::create_dir_all(temporary.path().join(".semantic-zed"))
            .expect("create project metadata directory");
        fs::write(temporary.path().join(PROJECT_METADATA_PATH), "{}")
            .expect("write project metadata");
        assert!(temporary.path().join(PROJECT_METADATA_PATH).is_file());
    }

    #[test]
    fn keeps_archived_and_trashed_projects_out_of_the_replica_creation_flow() {
        let active = RemoteProject {
            id: "active".to_string(),
            name: "Active paper".to_string(),
            access_level: "owner".to_string(),
            last_updated: Some("2026-08-09T12:00:00.000Z".to_string()),
            archived: false,
            trashed: false,
        };
        assert_eq!(active.bucket_label(), "My projects");
        assert!(!active.is_read_only_bucket());
        assert_eq!(
            active.updated_label().as_deref(),
            Some("Updated 2026-08-09")
        );

        let archived = RemoteProject {
            archived: true,
            ..active.clone()
        };
        assert_eq!(archived.bucket_label(), "Archived");
        assert!(archived.is_read_only_bucket());

        let trashed = RemoteProject {
            trashed: true,
            ..active
        };
        assert_eq!(trashed.bucket_label(), "Trash");
        assert!(trashed.is_read_only_bucket());
    }

    #[test]
    fn opens_an_existing_matching_replica_without_reinitializing_it() {
        let temporary = TempDir::new().expect("create local replica root");
        let metadata = temporary.path().join(".semantic-zed");
        fs::create_dir_all(&metadata).expect("create project metadata directory");
        fs::write(metadata.join("project.json"), r#"{"projectId":"paper-1"}"#)
            .expect("write project metadata");

        initialize_local_replica("paper-1", temporary.path())
            .expect("matching local replica remains unchanged");
        assert!(initialize_local_replica("paper-2", temporary.path()).is_err());
    }

    #[test]
    fn authenticates_control_requests_without_exposing_the_runtime_token() {
        let temporary = TempDir::new().expect("create temporary paper root");
        let metadata = temporary.path().join(".semantic-zed");
        fs::create_dir_all(&metadata).expect("create runtime metadata directory");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind control listener");
        let port = listener.local_addr().expect("read listener address").port();
        fs::write(
            metadata.join("runtime.json"),
            format!(r#"{{"host":"127.0.0.1","port":{port},"token":"test-token"}}"#),
        )
        .expect("write runtime metadata");

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept control connection");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone control stream"))
                .read_line(&mut request)
                .expect("read control request");
            let request: Value = serde_json::from_str(&request).expect("parse control request");
            assert_eq!(request["token"], "test-token");
            assert_eq!(request["method"], "status");
            writeln!(
                stream,
                r#"{{"jsonrpc":"2.0","id":"semantic-zed","result":{{"state":"live","documents":{{"pending":0}},"conflicts":[]}}}}"#
            )
            .expect("write control response");
        });

        let status = query_status(temporary.path()).expect("query daemon status");
        assert_eq!(status.summary(), "Overleaf live · 0 pending");
        server.join().expect("join control server");
    }

    #[test]
    fn ignores_notifications_before_the_matching_control_response() {
        let temporary = TempDir::new().expect("create temporary paper root");
        let metadata = temporary.path().join(".semantic-zed");
        fs::create_dir_all(&metadata).expect("create runtime metadata directory");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind control listener");
        let port = listener.local_addr().expect("read listener address").port();
        fs::write(
            metadata.join("runtime.json"),
            format!(r#"{{"host":"127.0.0.1","port":{port},"token":"test-token"}}"#),
        )
        .expect("write runtime metadata");

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept control connection");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone control stream"))
                .read_line(&mut request)
                .expect("read control request");
            writeln!(
                stream,
                r#"{{"jsonrpc":"2.0","method":"collaborator/changed","params":{{"cursors":[]}}}}"#
            )
            .expect("write interleaved notification");
            writeln!(
                stream,
                r#"{{"jsonrpc":"2.0","id":"semantic-zed","result":{{"state":"live","documents":{{"pending":0}},"conflicts":[]}}}}"#
            )
            .expect("write control response");
        });

        let status = query_status(temporary.path()).expect("query daemon status");
        assert_eq!(status.summary(), "Overleaf live · 0 pending");
        server.join().expect("join control server");
    }

    #[test]
    fn parses_initial_and_incremental_overleaf_presence() {
        let initial = presence_from_control_message(&json!({
            "jsonrpc": "2.0",
            "id": "semantic-zed-presence",
            "result": {
                "subscribed": true,
                "presence": [{
                    "clientId": "remote-1",
                    "name": "Remote Writer",
                    "documentPath": "main.tex",
                    "row": 2,
                    "column": 7
                }]
            }
        }))
        .expect("parse initial presence")
        .expect("initial presence message");
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].document_path, "main.tex");
        assert_eq!(initial[0].row, 2);

        let updated = presence_from_control_message(&json!({
            "jsonrpc": "2.0",
            "method": "collaborator/changed",
            "params": {
                "cursors": [{
                    "clientId": "remote-1",
                    "name": "Remote Writer",
                    "documentPath": "sections/results.tex",
                    "row": 8,
                    "column": 3
                }]
            }
        }))
        .expect("parse updated presence")
        .expect("updated presence message");
        assert_eq!(updated[0].document_path, "sections/results.tex");
        assert_eq!(updated[0].column, 3);
    }

    #[test]
    fn rejects_presence_paths_that_escape_the_replica() {
        let presence = presence_from_control_message(&json!({
            "jsonrpc": "2.0",
            "method": "collaborator/changed",
            "params": {
                "cursors": [{
                    "clientId": "remote-1",
                    "name": "Remote Writer",
                    "documentPath": "../private.tex",
                    "row": 0,
                    "column": 0
                }]
            }
        }))
        .expect("parse presence")
        .expect("presence notification");
        assert!(presence.is_empty());
    }
}
