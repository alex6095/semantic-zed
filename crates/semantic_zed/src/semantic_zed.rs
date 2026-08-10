use anyhow::{Context as _, Result, anyhow, bail};
use editor::{
    Editor, EditorElement, EditorEvent, EditorStyle, NavigationOverlayKey, NavigationOverlayLabel,
    NavigationTargetOverlay, SelectionEffects, scroll::Autoscroll,
};
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    PathPromptOptions, PromptLevel, Role, Styled, Subscription, Task, TextStyle, WeakEntity,
    Window, actions, prelude::*,
};
use gpui_tokio::Tokio;
use language::{Bias, Buffer, BufferEditSource, Point, PointUtf16, Unclipped};
use project::{DirectoryLister, Project};
use semantic_overleaf::{
    browser::authenticate_with_browser,
    credentials::CredentialStore,
    http::{Identity, OverleafHttpClient},
    sync::{
        NativePresence, NativeProjectConfig, NativeSnapshotMaterialization, NativeStatus,
        NativeSyncError, NativeSyncEvent, NativeSyncHandle,
    },
};
use serde::Deserialize;
use serde_json::Value;
use settings::Settings as _;
use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use theme_settings::ThemeSettings;
use ui::{
    Button, ButtonStyle, Color, ContextMenu, Icon, IconButton, IconName, IconSize, Label,
    LabelSize, ListItem, ListItemSpacing, PopoverMenu, TintColor, Tooltip, prelude::*,
};
use unicode_segmentation::UnicodeSegmentation;
use util::ResultExt as _;
use workspace::{
    OpenMode, OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

mod design;
mod pdf_preview;

use design::ScientificPalette;
pub use pdf_preview::PdfPreviewPanel;

const PAPER_PANEL_KEY: &str = "SemanticZedPaperPanel";
const PROJECT_METADATA_PATH: &str = ".semantic-zed/project.json";
const CURSOR_UPDATE_THROTTLE: Duration = Duration::from_millis(250);
const SAVE_COMPILE_DEBOUNCE: Duration = Duration::from_millis(350);
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
    project_action_task: Task<()>,
    presence_task: Task<()>,
    follow_task: Task<()>,
    compile_task: Task<()>,
    save_compile_task: Task<()>,
    saved_document_task: Task<()>,
    cursor_task: Task<()>,
    document_push_task: Task<()>,
    workspace: WeakEntity<Workspace>,
    projects: ProjectListState,
    initializing_project: Option<String>,
    project_message: Option<String>,
    linked_project_id: Option<String>,
    trashing_project_id: Option<String>,
    new_project_editor: Entity<Editor>,
    show_new_project_form: bool,
    creating_project: bool,
    presence: Vec<OverleafPresence>,
    active_editor_subscription: Option<Subscription>,
    cursor_worker_running: bool,
    pending_cursor: Option<LocalOverleafCursor>,
    last_cursor_sent: Option<LocalOverleafCursor>,
    last_cursor_sent_at: Option<Instant>,
    native_sync: Option<NativeSyncHandle>,
    native_sync_starting_root: Option<PathBuf>,
    pending_documents: HashMap<String, PendingDocumentSnapshot>,
    in_flight_documents: HashMap<String, String>,
    last_submitted_documents: HashMap<String, String>,
    document_push_worker_running: bool,
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
            project_action_task: Task::ready(()),
            presence_task: Task::ready(()),
            follow_task: Task::ready(()),
            compile_task: Task::ready(()),
            save_compile_task: Task::ready(()),
            saved_document_task: Task::ready(()),
            cursor_task: Task::ready(()),
            document_push_task: Task::ready(()),
            workspace: workspace_handle,
            projects: ProjectListState::NotConnected,
            initializing_project: None,
            project_message: None,
            linked_project_id: None,
            trashing_project_id: None,
            new_project_editor,
            show_new_project_form: false,
            creating_project: false,
            presence: Vec::new(),
            active_editor_subscription: None,
            cursor_worker_running: false,
            pending_cursor: None,
            last_cursor_sent: None,
            last_cursor_sent_at: None,
            native_sync: None,
            native_sync_starting_root: None,
            pending_documents: HashMap::default(),
            in_flight_documents: HashMap::default(),
            last_submitted_documents: HashMap::default(),
            document_push_worker_running: false,
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
        if paper_root != self.paper_root {
            self.reset_native_sync();
            self.reset_cursor_publisher();
        }
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
            self.reset_native_sync();
            self.paper_root = paper_root;
            self.reset_cursor_publisher();
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
                if let project::Event::BufferEdited { source } = event {
                    this.queue_project_document_updates(project, *source, window, cx);
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
            |this, workspace, event: &workspace::Event, window, cx| match event {
                workspace::Event::ActiveItemChanged => {
                    this.schedule_presence_markers(window, cx);
                    this.schedule_observe_active_editor(workspace, window, cx);
                }
                workspace::Event::UserSavedItem { item, .. } => {
                    if let Some(editor) = item.upgrade().and_then(|item| item.act_as::<Editor>(cx))
                        && this.editor_belongs_to_replica(&editor, cx)
                    {
                        this.mark_saved_editor_document(&editor, window, cx);
                        this.schedule_compile_after_save(window, cx);
                    }
                }
                _ => {}
            },
        ));
        self.schedule_observe_active_editor(workspace, window, cx);
    }

    fn schedule_observe_active_editor(
        &self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.entity().downgrade();
        let workspace = workspace.clone();
        window.defer(cx, move |window, cx| {
            panel
                .update(cx, |panel, cx| {
                    panel.observe_active_editor(&workspace, window, cx);
                })
                .ok();
        });
    }

    fn observe_active_editor(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_editor_subscription = None;
        let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
            return;
        };
        self.active_editor_subscription = Some(cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::SelectionsChanged { local: true }) {
                    this.queue_cursor_update(editor, window, cx);
                }
            },
        ));
        self.queue_cursor_update(&editor, window, cx);
    }

    fn editor_belongs_to_replica(&self, editor: &Entity<Editor>, cx: &App) -> bool {
        self.paper_root
            .as_ref()
            .and_then(|root| editor_relative_path(editor, root, cx))
            .is_some()
    }

    fn queue_active_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
            return;
        };
        self.queue_cursor_update(&editor, window, cx);
    }

    fn queue_cursor_update(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.status.is_live() {
            return;
        }
        let Some(root) = self.paper_root.as_ref() else {
            return;
        };
        let Some(handle) = self.native_sync.clone() else {
            return;
        };
        let Some(cursor) = local_cursor_for_editor(editor, root, cx) else {
            return;
        };
        if self.last_cursor_sent.as_ref() == Some(&cursor)
            || self.pending_cursor.as_ref() == Some(&cursor)
        {
            return;
        }
        self.pending_cursor = Some(cursor);
        if self.cursor_worker_running {
            return;
        }

        self.cursor_worker_running = true;
        let tokio = Tokio::handle(cx);
        self.cursor_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                let delay = this
                    .update(cx, |this, _| {
                        if this.pending_cursor.is_none() {
                            this.cursor_worker_running = false;
                            return None;
                        }
                        let elapsed = this
                            .last_cursor_sent_at
                            .map(|sent_at| sent_at.elapsed())
                            .unwrap_or(CURSOR_UPDATE_THROTTLE);
                        Some(CURSOR_UPDATE_THROTTLE.saturating_sub(elapsed))
                    })
                    .ok()
                    .flatten();
                let Some(delay) = delay else {
                    return;
                };
                if !delay.is_zero() {
                    cx.background_executor().timer(delay).await;
                }

                let cursor = this
                    .update(cx, |this, _| this.pending_cursor.take())
                    .ok()
                    .flatten();
                let Some(cursor) = cursor else {
                    continue;
                };
                let cursor_for_request = cursor.clone();
                let handle = handle.clone();
                let result = tokio
                    .spawn(async move {
                        handle
                            .update_position(
                                cursor_for_request.document_path,
                                cursor_for_request.row,
                                cursor_for_request.column,
                            )
                            .await
                    })
                    .await;
                this.update(cx, |this, _| {
                    this.last_cursor_sent_at = Some(Instant::now());
                    if result.is_ok_and(|result| result.is_ok()) {
                        this.last_cursor_sent = Some(cursor);
                    }
                })
                .ok();
            }
        });
    }

    fn refresh_current_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            self.linked_project_id = None;
            self.status = PaperStatus::Unavailable;
            self.reset_native_sync();
            self.clear_presence(window, cx);
            cx.notify();
            return;
        };

        self.linked_project_id = NativeProjectConfig::load(&root)
            .ok()
            .map(|config| config.project_id);

        self.status = PaperStatus::Loading;
        cx.notify();
        if let Some(handle) = self.native_sync.clone() {
            let status_task = Tokio::spawn_result(cx, async move {
                handle.status().await.map_err(anyhow::Error::from)
            });
            self.task = cx.spawn_in(window, async move |this, cx| {
                let status = status_task.await;
                this.update_in(cx, |this, window, cx| {
                    this.apply_native_status_result(status, window, cx);
                })
                .log_err();
            });
            return;
        }

        if self.native_sync_starting_root.as_ref() == Some(&root) {
            return;
        }
        self.native_sync_starting_root = Some(root.clone());

        let root_for_start = root.clone();
        let start_task = Tokio::spawn_result(cx, async move {
            let handle = NativeSyncHandle::start_from_root(root_for_start).await?;
            let status = handle.status().await?;
            Ok((handle, status))
        });
        self.task = cx.spawn_in(window, async move |this, cx| {
            let result = start_task.await;
            this.update_in(cx, |this, window, cx| {
                if this.native_sync_starting_root.as_ref() != Some(&root)
                    || this.paper_root.as_ref() != Some(&root)
                {
                    return;
                }
                this.native_sync_starting_root = None;
                match result {
                    Ok((handle, status)) => {
                        this.native_sync = Some(handle);
                        // Subscribe and reset the per-socket cursor publisher before
                        // applying the live status. Applying it queues the active
                        // editor caret; doing the reset afterwards used to cancel
                        // that first publication until the user moved again.
                        this.restart_native_event_stream(window, cx);
                        this.apply_native_status(status, window, cx);
                    }
                    Err(error) => {
                        this.status = PaperStatus::Error(error.to_string());
                        this.clear_presence(window, cx);
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn start_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.paper_root.is_none() {
            return;
        }
        self.refresh_current_root(window, cx);
    }

    fn apply_native_status_result(
        &mut self,
        result: Result<NativeStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(status) => self.apply_native_status(status, window, cx),
            Err(error) => {
                self.status = PaperStatus::Error(error.to_string());
                self.clear_presence(window, cx);
                cx.notify();
            }
        }
    }

    fn apply_native_status(
        &mut self,
        status: NativeStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_live = status.state == "live";
        self.linked_project_id = Some(status.project_id.clone());
        self.presence = status
            .collaborators
            .iter()
            .cloned()
            .map(OverleafPresence::from)
            .filter(OverleafPresence::is_valid)
            .collect();
        self.status = PaperStatus::Ready(DaemonStatus::from_native(&status));
        self.schedule_presence_markers(window, cx);
        if is_live {
            self.queue_active_cursor(window, cx);
        }
        cx.notify();
    }

    fn login_to_overleaf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.login.is_in_progress() {
            return;
        }

        self.login = LoginState::OpeningBrowser;
        cx.notify();
        let login_task = Tokio::spawn_result(cx, login_with_browser());
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
        if self.projects.is_loading() {
            return;
        }

        let previous = self.projects.projects().to_vec();
        self.projects = ProjectListState::Loading {
            previous: previous.clone(),
        };
        cx.notify();
        let list_task = Tokio::spawn_result(cx, list_remote_projects());
        self.projects_task = cx.spawn_in(window, async move |this, cx| {
            let projects = list_task.await;
            this.update_in(cx, |this, _, cx| {
                this.projects = ProjectListState::from_result(projects, previous);
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
        if self.initializing_project.is_some()
            || remote_project.is_read_only_bucket()
            || self.linked_project_id.as_deref() == Some(remote_project.id.as_str())
        {
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
        let project_id = remote_project.id;
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
                                // The panel in the opened workspace owns and starts the native
                                // Rust actor. Keeping ownership with that workspace prevents two
                                // sync engines from racing over one replica.
                                this.status = PaperStatus::Loading;
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
        let create_task = Tokio::spawn_result(cx, {
            let name = name.clone();
            async move { create_remote_project(&name).await }
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

    fn confirm_project_trash_action(
        &mut self,
        project: RemoteProject,
        action: ProjectTrashAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.trashing_project_id.is_some() || !action.is_available_for(&project) {
            return;
        }

        let (title, detail, confirm_label) = match action {
            ProjectTrashAction::MoveToTrash => (
                format!("Move {} to Trash?", project.name),
                "This removes the project from your active Overleaf list. The local replica is preserved, and the project can be restored from Trash.",
                "Move to Trash",
            ),
            ProjectTrashAction::Restore => (
                format!("Restore {}?", project.name),
                "This returns the project to your active Overleaf project list.",
                "Restore",
            ),
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(detail),
            &[confirm_label, "Cancel"],
            cx,
        );
        let project_id = project.id.clone();
        let project_name = project.name.clone();
        self.project_action_task = cx.spawn_in(window, async move |this, cx| {
            if prompt.await != Ok(0) {
                return;
            }

            let request = this.update(cx, |this, cx| {
                this.trashing_project_id = Some(project_id.clone());
                this.project_message = Some(action.progress_message(&project_name));
                cx.notify();
                let request_project_id = project_id.clone();
                Tokio::spawn_result(cx, async move {
                    update_remote_project_trash_state(&request_project_id, action).await
                })
            });
            let Ok(request) = request else {
                return;
            };
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.trashing_project_id = None;
                match result {
                    Ok(()) => {
                        this.project_message = Some(action.success_message(&project_name));
                        if matches!(action, ProjectTrashAction::MoveToTrash)
                            && this.linked_project_id.as_deref() == Some(project_id.as_str())
                        {
                            this.reset_native_sync();
                            this.clear_presence(window, cx);
                            this.status = PaperStatus::Error(
                                "This linked project is in Overleaf Trash. Restore it to resume live sync."
                                    .to_string(),
                            );
                        }
                        this.refresh_projects(window, cx);
                    }
                    Err(error) => {
                        this.project_message = Some(format!(
                            "Could not {} {}: {}",
                            action.failure_verb(),
                            project_name,
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
        self.start_compile(true, window, cx);
    }

    fn schedule_compile_after_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.status.is_live() {
            return;
        }
        self.save_compile_task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SAVE_COMPILE_DEBOUNCE).await;
            this.update_in(cx, |this, window, cx| {
                this.start_compile(false, window, cx);
            })
            .ok();
        });
    }

    fn mark_saved_editor_document(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.paper_root.as_ref() else {
            return;
        };
        let Some(handle) = self.native_sync.clone() else {
            return;
        };
        let Some(buffer) = editor.read(cx).active_buffer(cx) else {
            return;
        };
        let Some(path) = buffer_relative_path(&buffer, root, cx) else {
            return;
        };
        let text = buffer.read(cx).text();
        let tokio = Tokio::handle(cx);
        self.saved_document_task = cx.spawn_in(window, async move |this, cx| {
            let result = tokio
                .spawn(async move {
                    handle
                        .apply_snapshot(path.clone(), text.clone(), "editor-save")
                        .await
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(Ok(snapshot)) => {
                    this.last_submitted_documents
                        .insert(snapshot.path, snapshot.text);
                }
                Ok(Err(error)) => {
                    this.project_message = Some(format!(
                        "Could not confirm the saved editor file with Overleaf: {error}"
                    ));
                    cx.notify();
                }
                Err(error) => {
                    this.project_message =
                        Some(format!("The saved-file Overleaf task stopped: {error}"));
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn start_compile(&mut self, reveal_pdf: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            return;
        };
        let Some(handle) = self.native_sync.clone() else {
            return;
        };
        if !self.status.is_live() {
            return;
        }
        let open_documents = self
            .workspace
            .upgrade()
            .map(|workspace| {
                let project = workspace.read(cx).project().clone();
                open_document_snapshots(&project, &root, cx)
            })
            .unwrap_or_default();
        self.status = PaperStatus::Compiling;
        cx.notify();
        let compile_task = Tokio::spawn_result(cx, async move {
            for (path, text) in open_documents {
                match handle
                    .apply_editor_snapshot(path, text, "compile-barrier")
                    .await
                {
                    Ok(_) | Err(NativeSyncError::UnknownDocument(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            handle.compile(None).await.map_err(anyhow::Error::from)
        });
        self.compile_task = cx.spawn_in(window, async move |this, cx| {
            let status = compile_task.await;
            this.update_in(cx, |this, window, cx| {
                let compiled = status.is_ok();
                this.apply_native_status_result(status, window, cx);
                if compiled {
                    this.refresh_pdf(root, reveal_pdf, window, cx);
                }
            })
            .log_err();
        });
    }

    fn preview_pdf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            return;
        };
        self.refresh_pdf(root, true, window, cx);
    }

    fn refresh_pdf(
        &mut self,
        root: PathBuf,
        reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        // Revealing a dock panel asks the workspace to inspect all panel
        // handles, including this PaperPanel. Run after the current button
        // callback releases its GPUI entity lease to avoid a double-read panic.
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                if reveal {
                    workspace.reveal_panel::<PdfPreviewPanel>(window, cx);
                }
                if let Some(panel) = workspace.panel::<PdfPreviewPanel>(cx) {
                    panel.update(cx, |panel, cx| panel.open_for_root(root, window, cx));
                }
            });
        });
    }

    fn clear_presence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presence_task = Task::ready(());
        self.reset_cursor_publisher();
        if !self.presence.is_empty() {
            self.presence.clear();
            self.schedule_presence_markers(window, cx);
        }
    }

    fn restart_native_event_stream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.paper_root.clone() else {
            self.clear_presence(window, cx);
            return;
        };
        let Some(handle) = self.native_sync.clone() else {
            self.clear_presence(window, cx);
            return;
        };
        // A new Overleaf socket session has no cursor state, even if the local
        // caret has not moved. Publish it again after every reconnect.
        self.reset_cursor_publisher();
        let (sender, receiver) = async_channel::bounded(128);
        let mut native_events = handle.subscribe();
        let stream_task = Tokio::spawn(cx, async move {
            loop {
                match native_events.recv().await {
                    Ok(event) => {
                        if sender.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        self.presence_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this.paper_root.as_ref() != Some(&root) {
                            return;
                        }
                        this.handle_native_sync_event(event, window, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
            stream_task.await.log_err();
        });
    }

    fn handle_native_sync_event(
        &mut self,
        event: NativeSyncEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NativeSyncEvent::StatusChanged { status } => {
                self.apply_native_status(status, window, cx);
            }
            NativeSyncEvent::PresenceChanged { collaborators } => {
                let presence = collaborators
                    .into_iter()
                    .map(OverleafPresence::from)
                    .filter(OverleafPresence::is_valid)
                    .collect::<Vec<_>>();
                if self.presence != presence {
                    self.presence = presence;
                    self.schedule_presence_markers(window, cx);
                    cx.notify();
                }
            }
            NativeSyncEvent::DocumentChanged {
                path,
                text,
                materialized,
                ..
            } => {
                self.last_submitted_documents
                    .insert(path.clone(), text.clone());
                if !self.has_newer_local_document_snapshot(&path, &text) {
                    if materialized {
                        // Re-read a remote snapshot that the sync engine has
                        // already written to disk. This updates Zed's saved
                        // mtime/version as well as its text, preventing the
                        // next Cmd-S from reporting its own remote write as a
                        // conflicting external modification.
                        self.reload_open_document_from_remote(&path, &text, cx);
                    } else {
                        // An open editor owns this snapshot and its backing
                        // file can still be older. Apply it directly without
                        // asking Zed to reload stale disk bytes.
                        self.apply_open_document_change_from_remote(&path, text, cx);
                    }
                }
            }
            NativeSyncEvent::PdfChanged { .. } => {
                if let Some(root) = self.paper_root.clone() {
                    self.refresh_pdf(root, false, window, cx);
                }
            }
            NativeSyncEvent::Error { message } => {
                self.project_message = Some(message);
                cx.notify();
            }
        }
    }

    fn reset_cursor_publisher(&mut self) {
        self.cursor_task = Task::ready(());
        self.cursor_worker_running = false;
        self.pending_cursor = None;
        self.last_cursor_sent = None;
        self.last_cursor_sent_at = None;
    }

    fn reset_native_sync(&mut self) {
        self.native_sync = None;
        self.native_sync_starting_root = None;
        self.presence_task = Task::ready(());
        self.document_push_task = Task::ready(());
        self.pending_documents.clear();
        self.in_flight_documents.clear();
        self.last_submitted_documents.clear();
        self.document_push_worker_running = false;
    }

    fn has_newer_local_document_snapshot(&self, path: &str, text: &str) -> bool {
        self.pending_documents
            .get(path)
            .is_some_and(|snapshot| snapshot.text != text)
            || self
                .in_flight_documents
                .get(path)
                .is_some_and(|snapshot| snapshot != text)
    }

    fn reload_open_document_from_remote(
        &self,
        document_path: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.paper_root.as_ref() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        for buffer in project.read(cx).opened_buffers(cx) {
            if buffer_relative_path(&buffer, root, cx).as_deref() != Some(document_path)
                || buffer.read(cx).text() == text
            {
                continue;
            }
            let reload = buffer.update(cx, |buffer, cx| buffer.reload_from_remote(cx));
            cx.spawn(async move |_, _| {
                let _ = reload.await;
            })
            .detach();
        }
    }

    fn apply_open_document_change_from_remote(
        &self,
        document_path: &str,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.paper_root.as_ref() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        for buffer in project.read(cx).opened_buffers(cx) {
            if buffer_relative_path(&buffer, root, cx).as_deref() != Some(document_path)
                || buffer.read(cx).text() == text
            {
                continue;
            }
            let diff = buffer.update(cx, |buffer, cx| buffer.diff(text.clone(), cx));
            cx.spawn(async move |_, cx| {
                let diff = diff.await;
                buffer.update(cx, |buffer, cx| {
                    buffer.apply_diff_with_source(diff, BufferEditSource::Remote, cx);
                });
            })
            .detach();
        }
    }

    fn queue_project_document_updates(
        &mut self,
        project: &Entity<Project>,
        source: BufferEditSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (origin, materialization) = match source {
            BufferEditSource::User => ("editor", NativeSnapshotMaterialization::EditorBuffer),
            BufferEditSource::Agent => ("agent", NativeSnapshotMaterialization::EditorBuffer),
            BufferEditSource::External => ("external", NativeSnapshotMaterialization::Filesystem),
            BufferEditSource::Remote => return,
        };
        let Some(root) = self.paper_root.as_ref() else {
            return;
        };
        if self.native_sync.is_none() {
            return;
        }

        for buffer in project.read(cx).opened_buffers(cx) {
            let Some(path) = buffer_relative_path(&buffer, root, cx) else {
                continue;
            };
            let text = buffer.read(cx).text();
            if self.last_submitted_documents.get(&path) == Some(&text)
                || self
                    .pending_documents
                    .get(&path)
                    .is_some_and(|pending| pending.text == text)
            {
                continue;
            }
            self.pending_documents.insert(
                path,
                PendingDocumentSnapshot {
                    text,
                    origin,
                    materialization,
                },
            );
        }

        if self.pending_documents.is_empty() || self.document_push_worker_running {
            return;
        }
        self.document_push_worker_running = true;
        let tokio = Tokio::handle(cx);
        self.document_push_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(90))
                    .await;
                let batch = this
                    .update(cx, |this, _| {
                        let Some(handle) = this.native_sync.clone() else {
                            this.document_push_worker_running = false;
                            return None;
                        };
                        if this.pending_documents.is_empty() {
                            this.document_push_worker_running = false;
                            return None;
                        }
                        let documents = std::mem::take(&mut this.pending_documents);
                        this.in_flight_documents.extend(
                            documents
                                .iter()
                                .map(|(path, snapshot)| (path.clone(), snapshot.text.clone())),
                        );
                        Some((handle, documents))
                    })
                    .ok()
                    .flatten();
                let Some((handle, documents)) = batch else {
                    return;
                };
                let in_flight = documents
                    .iter()
                    .map(|(path, snapshot)| (path.clone(), snapshot.text.clone()))
                    .collect::<Vec<_>>();

                let result = tokio
                    .spawn(async move {
                        let mut submitted = Vec::with_capacity(documents.len());
                        for (path, snapshot) in documents {
                            handle
                                .apply_snapshot_with_materialization(
                                    path.clone(),
                                    snapshot.text.clone(),
                                    snapshot.origin,
                                    snapshot.materialization,
                                )
                                .await?;
                            submitted.push((path, snapshot.text));
                        }
                        Ok::<_, semantic_overleaf::sync::NativeSyncError>(submitted)
                    })
                    .await;

                if this
                    .update(cx, |this, cx| match result {
                        Ok(Ok(submitted)) => {
                            for (path, text) in submitted {
                                if this.in_flight_documents.get(&path) == Some(&text) {
                                    this.in_flight_documents.remove(&path);
                                }
                                this.last_submitted_documents.insert(path, text);
                            }
                        }
                        Ok(Err(error)) => {
                            this.clear_in_flight_documents(&in_flight);
                            this.project_message = Some(format!(
                                "Could not send an editor change to Overleaf: {error}"
                            ));
                            cx.notify();
                        }
                        Err(error) => {
                            this.clear_in_flight_documents(&in_flight);
                            this.project_message =
                                Some(format!("The native Overleaf edit task stopped: {error}"));
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn clear_in_flight_documents(&mut self, documents: &[(String, String)]) {
        for (path, text) in documents {
            if self.in_flight_documents.get(path) == Some(text) {
                self.in_flight_documents.remove(path);
            }
        }
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

    /// Follows a collaborator's latest published cursor. The remote position is
    /// UTF-16, just like the marker overlay, so resolve it only after the
    /// destination editor has loaded its current buffer snapshot.
    fn follow_collaborator(
        &mut self,
        collaborator: OverleafPresence,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.paper_root.as_deref() else {
            return;
        };
        let Some(path) = replica_document_path(root, &collaborator.document_path) else {
            self.project_message = Some(format!(
                "{}'s current file is not available in this replica.",
                collaborator.display_name()
            ));
            cx.notify();
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let open_task = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(
                path,
                OpenOptions {
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        self.follow_task = cx.spawn_in(window, async move |this, cx| {
            let item = match open_task.await {
                Ok(item) => item,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.project_message = Some(format!(
                            "Could not open {}: {error}",
                            collaborator.document_path
                        ));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let Some(editor) = item.downcast::<Editor>() else {
                return;
            };
            let point = PointUtf16::new(collaborator.row, collaborator.column);
            if editor
                .update_in(cx, |editor, window, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let point = snapshot.point_utf16_to_point(
                        snapshot.clip_point_utf16(Unclipped(point), Bias::Left),
                    );
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([point..point]),
                    );
                })
                .is_err()
            {
                return;
            }
            this.update(cx, |this, cx| {
                this.project_message = None;
                cx.notify();
            })
            .ok();
        });
    }

    fn status_text(&self) -> String {
        self.status.summary()
    }

    fn render_new_project_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = ScientificPalette::resolve(cx);
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
            .w_full()
            .min_w_0()
            .gap_2()
            .p_3()
            .bg(palette.card)
            .border_1()
            .border_color(palette.divider)
            .rounded_lg()
            .shadow(palette.card_shadow)
            .child(
                Label::new("New project")
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("Create on Overleaf, then choose a local folder.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .line_clamp(2),
            )
            .child(
                h_flex()
                    .w_full()
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
                    .w_full()
                    .min_w_0()
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
                                "Create"
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
        let palette = ScientificPalette::resolve(cx);
        let has_root = self.paper_root.is_some();
        let status_text = self.status_text();
        let login_text = self.login.summary();
        let login_button_label = self.login.button_label();
        let root_text = self
            .paper_root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No initialized Overleaf replica is open.".to_string());
        let linked_project_id = self
            .linked_project_id
            .clone()
            .or_else(|| match &self.status {
                PaperStatus::Ready(status) if !status.project_id.is_empty() => {
                    Some(status.project_id.clone())
                }
                _ => None,
            });
        let linked_remote_project = linked_project_id.as_deref().and_then(|project_id| {
            self.projects
                .projects()
                .iter()
                .find(|project| project.id == project_id)
        });
        let linked_project_name = match &self.status {
            PaperStatus::Ready(status) => status.project_name.clone(),
            _ => None,
        }
        .or_else(|| linked_remote_project.map(|project| project.name.clone()))
        .unwrap_or_else(|| {
            if has_root {
                "Linked Overleaf project".to_string()
            } else {
                "No Overleaf project linked".to_string()
            }
        });
        let linked_project_metadata = linked_project_id.as_deref().map(|project_id| {
            let short_id = short_project_id(project_id);
            let access = linked_remote_project
                .map(RemoteProject::access_label)
                .filter(|access| !access.is_empty());
            match access {
                Some(access) => format!("{access} · Project {short_id}"),
                None => format!("Project {short_id}"),
            }
        });

        let connected = matches!(&self.login, LoginState::Connected);
        let project_count = self.projects.projects().len();
        let presence_count = self.presence.len();
        let mut collaborator_rows = v_flex().gap_0p5();
        for collaborator in &self.presence {
            let participant_index = presence_participant_index(&collaborator.client_id);
            let participant_color = cx
                .theme()
                .players()
                .color_for_participant(participant_index)
                .cursor;
            let target = collaborator.clone();
            let collaborator_label = collaborator.display_name();
            let collaborator_tooltip = format!("Follow {collaborator_label}");
            let collaborator_initial = collaborator_initial(&collaborator_label);
            let avatar_text_color = if participant_color.l > 0.62 {
                gpui::black()
            } else {
                gpui::white()
            };
            collaborator_rows = collaborator_rows.child(
                ListItem::new(format!(
                    "semantic-zed-collaborator-{}",
                    collaborator.client_id
                ))
                .spacing(ListItemSpacing::Dense)
                .rounded()
                .aria_role(Role::Button)
                .aria_label(format!(
                    "Go to {} at {} line {}, column {}",
                    collaborator_label,
                    collaborator.document_path,
                    collaborator.row + 1,
                    collaborator.column + 1,
                ))
                .tooltip(Tooltip::text(collaborator_tooltip))
                .start_slot(
                    div()
                        .flex()
                        .flex_none()
                        .w(px(36.0))
                        .h(px(36.0))
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(participant_color)
                        .child(
                            Label::new(collaborator_initial)
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD)
                                .color(Color::Custom(avatar_text_color)),
                        ),
                )
                .end_slot(Icon::new(IconName::ChevronRight).size(IconSize::XSmall))
                .show_end_slot_on_hover()
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            Label::new(collaborator_label.clone())
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::MEDIUM)
                                .truncate(),
                        )
                        .child(
                            Label::new(format!(
                                "{} · {}:{}",
                                collaborator.document_path,
                                collaborator.row + 1,
                                collaborator.column + 1
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.follow_collaborator(target.clone(), window, cx);
                })),
            );
        }
        let mut project_rows = v_flex().id("semantic-zed-project-list").gap_0p5();
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
                    div().px_1().pt_2().pb_1().child(
                        Label::new(project.bucket_label())
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::MEDIUM)
                            .color(Color::Muted),
                    ),
                );
                previous_bucket = Some(project.bucket_label());
            }
            let is_current = linked_project_id.as_deref() == Some(project.id.as_str());
            let disabled = self.initializing_project.is_some()
                || self.creating_project
                || self.trashing_project_id.is_some();
            let status_label = if is_current {
                "Syncing".to_string()
            } else {
                project.access_label()
            };
            let updated_label = project.updated_label();
            let action_menu = project.trash_action().map(|action| {
                let panel = cx.weak_entity();
                let action_project = project.clone();
                let trigger_id = format!("semantic-zed-project-actions-{}", project.id);
                div()
                    .id(format!(
                        "semantic-zed-project-actions-wrapper-{}",
                        project.id
                    ))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        PopoverMenu::new(format!(
                            "semantic-zed-project-actions-menu-{}",
                            project.id
                        ))
                        .trigger(
                            IconButton::new(trigger_id, IconName::Ellipsis)
                                .icon_size(IconSize::Small)
                                .disabled(disabled)
                                .tooltip(Tooltip::text("Project actions")),
                        )
                        .menu(move |window, cx| {
                            let panel = panel.clone();
                            let action_project = action_project.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                let panel = panel.clone();
                                let action_project = action_project.clone();
                                menu.entry(action.label(), None, move |window, cx| {
                                    panel
                                        .update(cx, |this, cx| {
                                            this.confirm_project_trash_action(
                                                action_project.clone(),
                                                action,
                                                window,
                                                cx,
                                            );
                                        })
                                        .ok();
                                })
                            }))
                        }),
                    )
                    .into_any_element()
            });
            let mut project_row =
                ListItem::new(format!("semantic-zed-project-{}", project.id))
                    .spacing(ListItemSpacing::Dense)
                    .rounded()
                    .disabled(disabled)
                    .toggle_state(is_current)
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
                    .end_slot(Label::new(status_label).size(LabelSize::XSmall).color(
                        if is_current {
                            Color::Accent
                        } else {
                            Color::Muted
                        },
                    ))
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
                    }));
            if let Some(action_menu) = action_menu {
                project_row = project_row.end_slot_on_hover(action_menu);
            }
            project_rows = project_rows.child(project_row);
        }

        v_flex()
            .id("semantic-zed-paper-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_3()
            .gap_3()
            .bg(palette.sidebar)
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .justify_between()
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
                                    .disabled(
                                        !connected
                                            || self.creating_project
                                            || self.projects.is_loading(),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.refresh_projects(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap_2()
                    .p_3()
                    .bg(palette.card)
                    .border_1()
                    .border_color(palette.divider)
                    .rounded_lg()
                    .shadow(palette.card_shadow.clone())
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(
                                        if connected {
                                            cx.theme().status().success
                                        } else {
                                            cx.theme().colors().text_muted
                                        },
                                    ))
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
                                Button::new("semantic-zed-login", login_button_label)
                                    .style(if connected {
                                        ButtonStyle::Transparent
                                    } else {
                                        ButtonStyle::Tinted(TintColor::Accent)
                                    })
                                    .disabled(self.login.is_in_progress())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.login_to_overleaf(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        Label::new(login_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
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
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(self.projects.summary())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    ),
            )
            .when(self.show_new_project_form, |this| {
                this.child(self.render_new_project_form(cx))
            })
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
                    .gap_2()
                    .p_3()
                    .bg(palette.card)
                    .border_1()
                    .border_color(palette.divider)
                    .rounded_lg()
                    .shadow(palette.card_shadow)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .justify_between()
                            .gap_2()
                            .child(
                                Label::new("Current project")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_1()
                                    .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(
                                        if self.status.is_live() {
                                            cx.theme().status().success
                                        } else {
                                            cx.theme().colors().text_muted
                                        },
                                    ))
                                    .child(
                                        Label::new(if self.status.is_live() {
                                            "Live"
                                        } else {
                                            "Not live"
                                        })
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    ),
                            ),
                    )
                    .child(
                        Label::new(linked_project_name)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .when_some(linked_project_metadata, |this, metadata| {
                        this.child(
                            Label::new(metadata)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .child(
                        Label::new(status_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0p5()
                            .pt_1()
                            .border_t_1()
                            .border_color(palette.divider)
                            .child(
                                Label::new("Local folder")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(root_text)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .when(presence_count > 0, |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .pt_1()
                                .border_t_1()
                                .border_color(palette.divider)
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .child(
                                            Label::new("Collaborators")
                                                .size(LabelSize::XSmall)
                                                .weight(FontWeight::SEMIBOLD),
                                        )
                                        .child(
                                            Label::new(presence_count.to_string())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        ),
                                )
                                .child(collaborator_rows),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_wrap()
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

    fn trash_action(&self) -> Option<ProjectTrashAction> {
        if self.trashed {
            Some(ProjectTrashAction::Restore)
        } else if !self.archived {
            Some(ProjectTrashAction::MoveToTrash)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectTrashAction {
    MoveToTrash,
    Restore,
}

impl ProjectTrashAction {
    fn label(self) -> &'static str {
        match self {
            Self::MoveToTrash => "Move to Trash…",
            Self::Restore => "Restore",
        }
    }

    fn is_available_for(self, project: &RemoteProject) -> bool {
        match self {
            Self::MoveToTrash => !project.archived && !project.trashed,
            Self::Restore => project.trashed,
        }
    }

    fn progress_message(self, project_name: &str) -> String {
        match self {
            Self::MoveToTrash => format!("Moving {project_name} to Overleaf Trash…"),
            Self::Restore => format!("Restoring {project_name} from Overleaf Trash…"),
        }
    }

    fn success_message(self, project_name: &str) -> String {
        match self {
            Self::MoveToTrash => {
                format!("Moved {project_name} to Overleaf Trash. Its local replica was preserved.")
            }
            Self::Restore => format!("Restored {project_name} from Overleaf Trash."),
        }
    }

    fn failure_verb(self) -> &'static str {
        match self {
            Self::MoveToTrash => "move to Trash",
            Self::Restore => "restore",
        }
    }
}

#[derive(Clone, Debug)]
enum ProjectListState {
    NotConnected,
    Loading {
        previous: Vec<RemoteProject>,
    },
    Ready(Vec<RemoteProject>),
    Error {
        previous: Vec<RemoteProject>,
        message: String,
    },
}

impl ProjectListState {
    fn from_result(result: Result<Vec<RemoteProject>>, previous: Vec<RemoteProject>) -> Self {
        match result {
            Ok(projects) => Self::Ready(projects),
            Err(error) => Self::Error {
                previous,
                message: local_replica_error_message(&error),
            },
        }
    }

    fn summary(&self) -> &'static str {
        match self {
            Self::NotConnected => "Connect an Overleaf account to browse projects.",
            Self::Loading { previous } if previous.is_empty() => "Loading your Overleaf projects…",
            Self::Loading { .. } => "Refreshing Overleaf projects…",
            Self::Ready(projects) if projects.is_empty() => "No Overleaf projects found.",
            Self::Ready(_) => "Choose a project to create or open its local replica.",
            Self::Error { previous, .. } if previous.is_empty() => {
                "Could not load Overleaf projects."
            }
            Self::Error { .. } => "Showing the last update. Refresh failed.",
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Error { message, .. } => Some(message),
            _ => None,
        }
    }

    fn projects(&self) -> &[RemoteProject] {
        match self {
            Self::Ready(projects) => projects,
            Self::Loading { previous } | Self::Error { previous, .. } => previous,
            Self::NotConnected => &[],
        }
    }

    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

impl PaperStatus {
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

    fn is_live(&self) -> bool {
        matches!(self, Self::Ready(status) if status.is_live())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DaemonStatus {
    state: String,
    project_id: String,
    project_name: Option<String>,
    pending: usize,
    conflicts: Vec<String>,
    last_compile_status: Option<String>,
}

impl DaemonStatus {
    fn from_native(status: &NativeStatus) -> Self {
        let last_compile_status = status
            .last_compile
            .as_ref()
            .and_then(|compile| compile.get("status"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        Self {
            state: status.state.clone(),
            project_id: status.project_id.clone(),
            project_name: status.project_name.clone(),
            pending: status.documents.pending,
            conflicts: status
                .conflicts
                .iter()
                .map(|conflict| conflict.path.clone())
                .collect(),
            last_compile_status,
        }
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

impl From<NativePresence> for OverleafPresence {
    fn from(presence: NativePresence) -> Self {
        Self {
            client_id: presence.client_id,
            name: presence.name,
            document_path: presence.document_path,
            row: presence.row,
            column: presence.column,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalOverleafCursor {
    document_path: String,
    row: u32,
    column: u32,
}

#[derive(Clone, Debug)]
struct PendingDocumentSnapshot {
    text: String,
    origin: &'static str,
    materialization: NativeSnapshotMaterialization,
}

async fn login_with_browser() -> Result<()> {
    authenticate_with_browser(OVERLEAF_SERVER).await?;
    Ok(())
}

fn saved_login_exists() -> Result<bool> {
    Ok(CredentialStore::default().load(OVERLEAF_SERVER)?.is_some())
}

async fn native_http_client() -> Result<(OverleafHttpClient, Identity)> {
    let record = CredentialStore::default()
        .load(OVERLEAF_SERVER)?
        .ok_or_else(|| anyhow!("No saved Overleaf login exists."))?;
    let saved_identity = record.identity;
    let mut client = OverleafHttpClient::new(OVERLEAF_SERVER)?;
    client.set_identity(saved_identity.clone());
    Ok((client, saved_identity))
}

fn persist_rotated_identity(client: &OverleafHttpClient, saved_identity: &Identity) -> Result<()> {
    let Some(current_identity) = client.identity() else {
        return Ok(());
    };
    if current_identity != saved_identity {
        CredentialStore::default().save(OVERLEAF_SERVER, current_identity.clone())?;
    }
    Ok(())
}

async fn list_remote_projects() -> Result<Vec<RemoteProject>> {
    let (mut client, saved_identity) = native_http_client().await?;
    let projects = client.list_projects().await?;
    persist_rotated_identity(&client, &saved_identity)?;
    projects
        .into_iter()
        .map(|mut value| {
            let object = value
                .as_object_mut()
                .ok_or_else(|| anyhow!("Overleaf returned a non-object project."))?;
            if !object.contains_key("accessLevel")
                && let Some(access) = object.get("source").cloned()
            {
                object.insert("accessLevel".into(), access);
            }
            let raw_status = object
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let archived = object
                .get("archived")
                .or_else(|| object.get("isArchived"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || raw_status == "archived";
            let trashed = object
                .get("trashed")
                .or_else(|| object.get("isTrashed"))
                .or_else(|| object.get("deleted"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || matches!(raw_status, "trashed" | "deleted");
            object.insert("archived".into(), Value::Bool(archived));
            object.insert("trashed".into(), Value::Bool(trashed));
            serde_json::from_value(value).context("parsing an Overleaf project")
        })
        .collect()
}

async fn create_remote_project(name: &str) -> Result<()> {
    let (mut client, saved_identity) = native_http_client().await?;
    client.create_project(name, "none").await?;
    persist_rotated_identity(&client, &saved_identity)?;
    Ok(())
}

async fn update_remote_project_trash_state(
    project_id: &str,
    action: ProjectTrashAction,
) -> Result<()> {
    let (mut client, saved_identity) = native_http_client().await?;
    match action {
        ProjectTrashAction::MoveToTrash => client.trash_project(project_id).await?,
        ProjectTrashAction::Restore => client.untrash_project(project_id).await?,
    }
    persist_rotated_identity(&client, &saved_identity)?;
    Ok(())
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
    let has_unrelated_entries = fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name() != ".DS_Store");
    if has_unrelated_entries {
        bail!("Target directory is not empty.");
    }
    NativeProjectConfig::new(project_id)?.save(root)?;
    Ok(())
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

fn apply_presence_markers(
    workspace: &mut Workspace,
    root: &Path,
    presence: &[OverleafPresence],
    cx: &mut Context<Workspace>,
) {
    let editors = workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>();
    for editor in editors {
        let relative_path = editor_relative_path(&editor, root, cx);
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
                    // Overleaf's CodeMirror client publishes line-relative
                    // JavaScript string offsets, i.e. UTF-16 code units. Zed's
                    // `Point` columns are UTF-8 byte offsets, so feeding the
                    // raw column to `clip_point` moves the caret into or past
                    // non-ASCII characters. Convert through the native UTF-16
                    // point API before creating the overlay anchor.
                    let utf16_point = snapshot.clip_point_utf16(
                        Unclipped(PointUtf16::new(cursor.row, cursor.column)),
                        Bias::Left,
                    );
                    let point = snapshot.point_utf16_to_point(utf16_point);
                    let anchor = snapshot.anchor_after(point);
                    let participant_index = presence_participant_index(&cursor.client_id);
                    NavigationTargetOverlay {
                        target_range: anchor..anchor,
                        label: NavigationOverlayLabel {
                            // Keep names out of the text canvas. The matching
                            // color and identity live in the Collaborators card.
                            text: "".into(),
                            text_color: cx
                                .theme()
                                .players()
                                .color_for_participant(participant_index)
                                .cursor,
                            x_offset: gpui::Pixels::ZERO,
                            scale_factor: 1.0,
                            caret_width: Some(gpui::px(1.0)),
                        },
                        covered_text_range: None,
                    }
                })
                .collect();
            editor.set_navigation_overlays(OVERLEAF_PRESENCE_OVERLAY_KEY, overlays, cx);
        });
    }
}

fn editor_relative_path(editor: &Entity<Editor>, root: &Path, cx: &App) -> Option<String> {
    let buffer = editor.read(cx).active_buffer(cx)?;
    buffer_relative_path(&buffer, root, cx)
}

fn buffer_relative_path(buffer: &Entity<Buffer>, root: &Path, cx: &App) -> Option<String> {
    let absolute_path = buffer.read(cx).file().and_then(|file| {
        let local = file.as_local()?;
        Some(local.abs_path(cx))
    })?;
    absolute_path
        .strip_prefix(root)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .filter(|path| !path.is_empty())
}

/// Resolves a server-provided document path only when it stays inside the
/// linked replica and materialized as a regular file. `OverleafPresence` also
/// validates its slash-separated form, while this component check keeps the
/// navigation path safe on Windows as well.
fn replica_document_path(root: &Path, document_path: &str) -> Option<PathBuf> {
    let relative = Path::new(document_path);
    if document_path.is_empty()
        || !relative.is_relative()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let path = root.join(relative);
    path.is_file().then_some(path)
}

fn open_document_snapshots(
    project: &Entity<Project>,
    root: &Path,
    cx: &App,
) -> Vec<(String, String)> {
    project
        .read(cx)
        .opened_buffers(cx)
        .into_iter()
        .filter_map(|buffer| {
            let path = buffer_relative_path(&buffer, root, cx)?;
            Some((path, buffer.read(cx).text()))
        })
        .collect()
}

fn local_cursor_for_editor(
    editor: &Entity<Editor>,
    root: &Path,
    cx: &mut App,
) -> Option<LocalOverleafCursor> {
    let document_path = editor_relative_path(editor, root, cx)?;
    let point = editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let point = editor.selections.newest::<Point>(&snapshot).head();
        snapshot.buffer_snapshot().point_to_point_utf16(point)
    });
    Some(LocalOverleafCursor {
        document_path,
        row: point.row,
        column: point.column,
    })
}

fn presence_participant_index(client_id: &str) -> u32 {
    client_id.bytes().fold(2_166_136_261, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}

fn collaborator_initial(name: &str) -> String {
    name.trim()
        .graphemes(true)
        .find(|grapheme| grapheme.chars().any(char::is_alphanumeric))
        .or_else(|| name.trim().graphemes(true).next())
        .map(|grapheme| grapheme.to_uppercase())
        .unwrap_or_else(|| "?".to_string())
}

fn short_project_id(project_id: &str) -> String {
    let characters = project_id.chars().collect::<Vec<_>>();
    if characters.len() <= 14 {
        return project_id.to_string();
    }
    format!(
        "{}…{}",
        characters[..8].iter().collect::<String>(),
        characters[characters.len() - 4..]
            .iter()
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reports_each_safety_relevant_connection_state() {
        let live = DaemonStatus {
            state: "live".to_string(),
            project_id: "paper-1".to_string(),
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
    fn exposes_only_recoverable_project_trash_actions() {
        let active = RemoteProject {
            id: "active".to_string(),
            name: "Active paper".to_string(),
            access_level: "readWrite".to_string(),
            last_updated: None,
            archived: false,
            trashed: false,
        };
        assert_eq!(active.trash_action(), Some(ProjectTrashAction::MoveToTrash));

        let archived = RemoteProject {
            archived: true,
            ..active.clone()
        };
        assert_eq!(archived.trash_action(), None);

        let trashed = RemoteProject {
            trashed: true,
            ..active
        };
        assert_eq!(trashed.trash_action(), Some(ProjectTrashAction::Restore));
    }

    #[test]
    fn renders_unicode_safe_collaborator_initials_and_project_ids() {
        assert_eq!(collaborator_initial("Alex Lee"), "A");
        assert_eq!(collaborator_initial(" 나영주"), "나");
        assert_eq!(collaborator_initial("👩🏽‍💻 Researcher"), "R");
        assert_eq!(collaborator_initial("✨"), "✨");
        assert_eq!(collaborator_initial("  "), "?");

        assert_eq!(short_project_id("paper-1"), "paper-1");
        assert_eq!(
            short_project_id("6a78e74ad29d6694dc52f58a"),
            "6a78e74a…f58a"
        );
    }

    #[test]
    fn refresh_keeps_the_last_successful_project_list_visible() {
        let previous = vec![RemoteProject {
            id: "paper-1".to_string(),
            name: "Last known paper".to_string(),
            access_level: "owner".to_string(),
            last_updated: None,
            archived: false,
            trashed: false,
        }];
        let loading = ProjectListState::Loading {
            previous: previous.clone(),
        };
        assert_eq!(loading.projects()[0].id, "paper-1");
        assert_eq!(loading.summary(), "Refreshing Overleaf projects…");

        let failed =
            ProjectListState::from_result(Err(anyhow!("temporary network failure")), previous);
        assert_eq!(failed.projects()[0].name, "Last known paper");
        assert_eq!(failed.summary(), "Showing the last update. Refresh failed.");
        assert_eq!(failed.detail(), Some("temporary network failure"));
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
    fn validates_native_presence_before_rendering_it() {
        let valid = OverleafPresence::from(NativePresence {
            client_id: "remote-1".into(),
            name: "Remote Writer".into(),
            document_path: "sections/results.tex".into(),
            row: 8,
            column: 3,
        });
        assert!(valid.is_valid());

        let escaping = OverleafPresence::from(NativePresence {
            document_path: "../private.tex".into(),
            ..NativePresence {
                client_id: "remote-2".into(),
                name: "Remote Writer".into(),
                document_path: String::new(),
                row: 0,
                column: 0,
            }
        });
        assert!(!escaping.is_valid());
    }

    #[test]
    fn resolves_collaborator_navigation_only_inside_materialized_replica_files() {
        let replica = TempDir::new().expect("create replica root");
        let sections = replica.path().join("sections");
        fs::create_dir_all(&sections).expect("create section directory");
        let document = sections.join("results.tex");
        fs::write(&document, "\\section{Results}\n").expect("write replica document");

        assert_eq!(
            replica_document_path(replica.path(), "sections/results.tex"),
            Some(document)
        );
        assert!(replica_document_path(replica.path(), "sections/missing.tex").is_none());
        assert!(replica_document_path(replica.path(), "../outside.tex").is_none());
        assert!(replica_document_path(replica.path(), "/tmp/outside.tex").is_none());
    }

    #[test]
    fn overleaf_cursor_columns_are_utf16_not_utf8_bytes() {
        let text = language::Rope::from("a한😀z\nsecond".to_string());
        let overleaf_point = PointUtf16::new(0, 4);
        let zed_point = text.point_utf16_to_point(overleaf_point);

        // `a` + `한` + `😀` occupy 4 UTF-16 code units but 8 UTF-8 bytes.
        assert_eq!(zed_point, Point::new(0, 8));
        assert_eq!(text.point_to_point_utf16(zed_point), overleaf_point);
    }
}
