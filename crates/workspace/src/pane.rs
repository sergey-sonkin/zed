use crate::{
    CloseWindow, NewFile, NewTerminal, OpenInTerminal, OpenOptions, OpenTerminal, OpenVisible,
    SplitDirection, ToggleFileFinder, ToggleProjectSymbols, ToggleZoom, Workspace,
    WorkspaceItemBuilder,
    item::{
        ActivateOnClose, ClosePosition, Item, ItemHandle, ItemSettings, PreviewTabsSettings,
        ProjectItemKind, SaveOptions, ShowCloseButton, ShowDiagnostics, TabContentParams,
        TabTooltipContent, WeakItemHandle,
    },
    move_item,
    notifications::NotifyResultExt,
    toolbar::Toolbar,
    workspace_settings::{AutosaveSetting, TabBarSettings, WorkspaceSettings},
};
use anyhow::Result;
use collections::{BTreeSet, HashMap, HashSet, VecDeque};
use futures::{StreamExt, stream::FuturesUnordered};
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, ClipboardItem, Context, Corner, Div,
    DragMoveEvent, Entity, EntityId, EventEmitter, ExternalPaths, FocusHandle, FocusOutEvent,
    Focusable, KeyContext, MouseButton, MouseDownEvent, NavigationDirection, Pixels, Point,
    PromptLevel, Render, ScrollHandle, Subscription, Task, WeakEntity, WeakFocusHandle, Window,
    actions, anchored, deferred, prelude::*,
};
use itertools::Itertools;
use language::DiagnosticSeverity;
use parking_lot::Mutex;
use project::{DirectoryLister, Project, ProjectEntryId, ProjectPath, WorktreeId};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{Settings, SettingsStore};
use std::{
    any::Any,
    cmp, fmt, mem,
    num::NonZeroUsize,
    ops::ControlFlow,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use theme::ThemeSettings;
use ui::{
    ButtonSize, Color, ContextMenu, ContextMenuEntry, ContextMenuItem, DecoratedIcon, IconButton,
    IconButtonShape, IconDecoration, IconDecorationKind, IconName, IconSize, Indicator, Label,
    PopoverMenu, PopoverMenuHandle, ScrollableHandle, Tab, TabBar, TabPosition, Tooltip,
    prelude::*, right_click_menu,
};
use util::{ResultExt, debug_panic, maybe, truncate_and_remove_front};

/// A selected entry in e.g. project panel.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectedEntry {
    pub worktree_id: WorktreeId,
    pub entry_id: ProjectEntryId,
}

/// A group of selected entries from project panel.
#[derive(Debug)]
pub struct DraggedSelection {
    pub active_selection: SelectedEntry,
    pub marked_selections: Arc<BTreeSet<SelectedEntry>>,
}

impl DraggedSelection {
    pub fn items<'a>(&'a self) -> Box<dyn Iterator<Item = &'a SelectedEntry> + 'a> {
        if self.marked_selections.contains(&self.active_selection) {
            Box::new(self.marked_selections.iter())
        } else {
            Box::new(std::iter::once(&self.active_selection))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SaveIntent {
    /// write all files (even if unchanged)
    /// prompt before overwriting on-disk changes
    Save,
    /// same as Save, but without auto formatting
    SaveWithoutFormat,
    /// write any files that have local changes
    /// prompt before overwriting on-disk changes
    SaveAll,
    /// always prompt for a new path
    SaveAs,
    /// prompt "you have unsaved changes" before writing
    Close,
    /// write all dirty files, don't prompt on conflict
    Overwrite,
    /// skip all save-related behavior
    Skip,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
pub struct ActivateItem(pub usize);

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct CloseActiveItem {
    pub save_intent: Option<SaveIntent>,
    #[serde(default)]
    pub close_pinned: bool,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct CloseInactiveItems {
    pub save_intent: Option<SaveIntent>,
    #[serde(default)]
    pub close_pinned: bool,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct CloseAllItems {
    pub save_intent: Option<SaveIntent>,
    #[serde(default)]
    pub close_pinned: bool,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct CloseCleanItems {
    #[serde(default)]
    pub close_pinned: bool,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct CloseItemsToTheRight {
    #[serde(default)]
    pub close_pinned: bool,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct CloseItemsToTheLeft {
    #[serde(default)]
    pub close_pinned: bool,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct RevealInProjectPanel {
    #[serde(skip)]
    pub entry_id: Option<u64>,
}

#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, Action)]
#[action(namespace = pane)]
#[serde(deny_unknown_fields)]
pub struct DeploySearch {
    #[serde(default)]
    pub replace_enabled: bool,
    #[serde(default)]
    pub included_files: Option<String>,
    #[serde(default)]
    pub excluded_files: Option<String>,
}

actions!(
    pane,
    [
        ActivatePreviousItem,
        ActivateNextItem,
        ActivateLastItem,
        AlternateFile,
        GoBack,
        GoForward,
        JoinIntoNext,
        JoinAll,
        ReopenClosedItem,
        SplitLeft,
        SplitUp,
        SplitRight,
        SplitDown,
        SplitHorizontal,
        SplitVertical,
        SwapItemLeft,
        SwapItemRight,
        TogglePreviewTab,
        TogglePinTab,
        UnpinAllTabs,
    ]
);

impl DeploySearch {
    pub fn find() -> Self {
        Self {
            replace_enabled: false,
            included_files: None,
            excluded_files: None,
        }
    }
}

const MAX_NAVIGATION_HISTORY_LEN: usize = 1024;

pub enum Event {
    AddItem {
        item: Box<dyn ItemHandle>,
    },
    ActivateItem {
        local: bool,
        focus_changed: bool,
    },
    Remove {
        focus_on_pane: Option<Entity<Pane>>,
    },
    RemoveItem {
        idx: usize,
    },
    RemovedItem {
        item: Box<dyn ItemHandle>,
    },
    Split(SplitDirection),
    ItemPinned,
    ItemUnpinned,
    JoinAll,
    JoinIntoNext,
    ChangeItemTitle,
    Focus,
    ZoomIn,
    ZoomOut,
    UserSavedItem {
        item: Box<dyn WeakItemHandle>,
        save_intent: SaveIntent,
    },
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::AddItem { item } => f
                .debug_struct("AddItem")
                .field("item", &item.item_id())
                .finish(),
            Event::ActivateItem { local, .. } => f
                .debug_struct("ActivateItem")
                .field("local", local)
                .finish(),
            Event::Remove { .. } => f.write_str("Remove"),
            Event::RemoveItem { idx } => f.debug_struct("RemoveItem").field("idx", idx).finish(),
            Event::RemovedItem { item } => f
                .debug_struct("RemovedItem")
                .field("item", &item.item_id())
                .finish(),
            Event::Split(direction) => f
                .debug_struct("Split")
                .field("direction", direction)
                .finish(),
            Event::JoinAll => f.write_str("JoinAll"),
            Event::JoinIntoNext => f.write_str("JoinIntoNext"),
            Event::ChangeItemTitle => f.write_str("ChangeItemTitle"),
            Event::Focus => f.write_str("Focus"),
            Event::ZoomIn => f.write_str("ZoomIn"),
            Event::ZoomOut => f.write_str("ZoomOut"),
            Event::UserSavedItem { item, save_intent } => f
                .debug_struct("UserSavedItem")
                .field("item", &item.id())
                .field("save_intent", save_intent)
                .finish(),
            Event::ItemPinned => f.write_str("ItemPinned"),
            Event::ItemUnpinned => f.write_str("ItemUnpinned"),
        }
    }
}

/// A container for 0 to many items that are open in the workspace.
/// Treats all items uniformly via the [`ItemHandle`] trait, whether it's an editor, search results multibuffer, terminal or something else,
/// responsible for managing item tabs, focus and zoom states and drag and drop features.
/// Can be split, see `PaneGroup` for more details.
pub struct Pane {
    alternate_file_items: (
        Option<Box<dyn WeakItemHandle>>,
        Option<Box<dyn WeakItemHandle>>,
    ),
    focus_handle: FocusHandle,
    items: Vec<Box<dyn ItemHandle>>,
    activation_history: Vec<ActivationHistoryEntry>,
    next_activation_timestamp: Arc<AtomicUsize>,
    zoomed: bool,
    was_focused: bool,
    active_item_index: usize,
    preview_item_id: Option<EntityId>,
    last_focus_handle_by_item: HashMap<EntityId, WeakFocusHandle>,
    nav_history: NavHistory,
    toolbar: Entity<Toolbar>,
    pub(crate) workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    pub drag_split_direction: Option<SplitDirection>,
    can_drop_predicate: Option<Arc<dyn Fn(&dyn Any, &mut Window, &mut App) -> bool>>,
    custom_drop_handle: Option<
        Arc<dyn Fn(&mut Pane, &dyn Any, &mut Window, &mut Context<Pane>) -> ControlFlow<(), ()>>,
    >,
    can_split_predicate:
        Option<Arc<dyn Fn(&mut Self, &dyn Any, &mut Window, &mut Context<Self>) -> bool>>,
    can_toggle_zoom: bool,
    should_display_tab_bar: Rc<dyn Fn(&Window, &mut Context<Pane>) -> bool>,
    render_tab_bar_buttons: Rc<
        dyn Fn(
            &mut Pane,
            &mut Window,
            &mut Context<Pane>,
        ) -> (Option<AnyElement>, Option<AnyElement>),
    >,
    render_tab_bar: Rc<dyn Fn(&mut Pane, &mut Window, &mut Context<Pane>) -> AnyElement>,
    show_tab_bar_buttons: bool,
    max_tabs: Option<NonZeroUsize>,
    _subscriptions: Vec<Subscription>,
    tab_bar_scroll_handle: ScrollHandle,
    /// Is None if navigation buttons are permanently turned off (and should not react to setting changes).
    /// Otherwise, when `display_nav_history_buttons` is Some, it determines whether nav buttons should be displayed.
    display_nav_history_buttons: Option<bool>,
    double_click_dispatch_action: Box<dyn Action>,
    save_modals_spawned: HashSet<EntityId>,
    close_pane_if_empty: bool,
    pub new_item_context_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub split_item_context_menu_handle: PopoverMenuHandle<ContextMenu>,
    pinned_tab_count: usize,
    diagnostics: HashMap<ProjectPath, DiagnosticSeverity>,
    zoom_out_on_close: bool,
    /// If a certain project item wants to get recreated with specific data, it can persist its data before the recreation here.
    pub project_item_restoration_data: HashMap<ProjectItemKind, Box<dyn Any + Send>>,
}

pub struct ActivationHistoryEntry {
    pub entity_id: EntityId,
    pub timestamp: usize,
}

pub struct ItemNavHistory {
    history: NavHistory,
    item: Arc<dyn WeakItemHandle>,
    is_preview: bool,
}

#[derive(Clone)]
pub struct NavHistory(Arc<Mutex<NavHistoryState>>);

struct NavHistoryState {
    mode: NavigationMode,
    backward_stack: VecDeque<NavigationEntry>,
    forward_stack: VecDeque<NavigationEntry>,
    closed_stack: VecDeque<NavigationEntry>,
    paths_by_item: HashMap<EntityId, (ProjectPath, Option<PathBuf>)>,
    pane: WeakEntity<Pane>,
    next_timestamp: Arc<AtomicUsize>,
}

#[derive(Debug, Copy, Clone)]
pub enum NavigationMode {
    Normal,
    GoingBack,
    GoingForward,
    ClosingItem,
    ReopeningClosedItem,
    Disabled,
}

impl Default for NavigationMode {
    fn default() -> Self {
        Self::Normal
    }
}

pub struct NavigationEntry {
    pub item: Arc<dyn WeakItemHandle>,
    pub data: Option<Box<dyn Any + Send>>,
    pub timestamp: usize,
    pub is_preview: bool,
}

#[derive(Clone)]
pub struct DraggedTab {
    pub pane: Entity<Pane>,
    pub item: Box<dyn ItemHandle>,
    pub ix: usize,
    pub detail: usize,
    pub is_active: bool,
}

impl EventEmitter<Event> for Pane {}

pub enum Side {
    Left,
    Right,
}

#[derive(Copy, Clone)]
enum PinOperation {
    Pin,
    Unpin,
}

impl Pane {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        next_timestamp: Arc<AtomicUsize>,
        can_drop_predicate: Option<Arc<dyn Fn(&dyn Any, &mut Window, &mut App) -> bool + 'static>>,
        double_click_dispatch_action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, Pane::focus_in),
            cx.on_focus_in(&focus_handle, window, Pane::focus_in),
            cx.on_focus_out(&focus_handle, window, Pane::focus_out),
            cx.observe_global_in::<SettingsStore>(window, Self::settings_changed),
            cx.subscribe(&project, Self::project_events),
        ];

        let handle = cx.entity().downgrade();

        Self {
            alternate_file_items: (None, None),
            focus_handle,
            items: Vec::new(),
            activation_history: Vec::new(),
            next_activation_timestamp: next_timestamp.clone(),
            was_focused: false,
            zoomed: false,
            active_item_index: 0,
            preview_item_id: None,
            max_tabs: WorkspaceSettings::get_global(cx).max_tabs,
            last_focus_handle_by_item: Default::default(),
            nav_history: NavHistory(Arc::new(Mutex::new(NavHistoryState {
                mode: NavigationMode::Normal,
                backward_stack: Default::default(),
                forward_stack: Default::default(),
                closed_stack: Default::default(),
                paths_by_item: Default::default(),
                pane: handle.clone(),
                next_timestamp,
            }))),
            toolbar: cx.new(|_| Toolbar::new()),
            tab_bar_scroll_handle: ScrollHandle::new(),
            drag_split_direction: None,
            workspace,
            project: project.downgrade(),
            can_drop_predicate,
            custom_drop_handle: None,
            can_split_predicate: None,
            can_toggle_zoom: true,
            should_display_tab_bar: Rc::new(|_, cx| TabBarSettings::get_global(cx).show),
            render_tab_bar_buttons: Rc::new(default_render_tab_bar_buttons),
            render_tab_bar: Rc::new(Self::render_tab_bar),
            show_tab_bar_buttons: TabBarSettings::get_global(cx).show_tab_bar_buttons,
            display_nav_history_buttons: Some(
                TabBarSettings::get_global(cx).show_nav_history_buttons,
            ),
            _subscriptions: subscriptions,
            double_click_dispatch_action,
            save_modals_spawned: HashSet::default(),
            close_pane_if_empty: true,
            split_item_context_menu_handle: Default::default(),
            new_item_context_menu_handle: Default::default(),
            pinned_tab_count: 0,
            diagnostics: Default::default(),
            zoom_out_on_close: true,
            project_item_restoration_data: HashMap::default(),
        }
    }

    fn alternate_file(&mut self, window: &mut Window, cx: &mut Context<Pane>) {
        let (_, alternative) = &self.alternate_file_items;
        if let Some(alternative) = alternative {
            let existing = self
                .items()
                .find_position(|item| item.item_id() == alternative.id());
            if let Some((ix, _)) = existing {
                self.activate_item(ix, true, true, window, cx);
            } else if let Some(upgraded) = alternative.upgrade() {
                self.add_item(upgraded, true, true, None, window, cx);
            }
        }
    }

    pub fn track_alternate_file_items(&mut self) {
        if let Some(item) = self.active_item().map(|item| item.downgrade_item()) {
            let (current, _) = &self.alternate_file_items;
            match current {
                Some(current) => {
                    if current.id() != item.id() {
                        self.alternate_file_items =
                            (Some(item), self.alternate_file_items.0.take());
                    }
                }
                None => {
                    self.alternate_file_items = (Some(item), None);
                }
            }
        }
    }

    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        // We not only check whether our focus handle contains focus, but also
        // whether the active item might have focus, because we might have just activated an item
        // that hasn't rendered yet.
        // Before the next render, we might transfer focus
        // to the item, and `focus_handle.contains_focus` returns false because the `active_item`
        // is not hooked up to us in the dispatch tree.
        self.focus_handle.contains_focused(window, cx)
            || self.active_item().map_or(false, |item| {
                item.item_focus_handle(cx).contains_focused(window, cx)
            })
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.was_focused {
            self.was_focused = true;
            self.update_history(self.active_item_index);
            cx.emit(Event::Focus);
            cx.notify();
        }

        self.toolbar.update(cx, |toolbar, cx| {
            toolbar.focus_changed(true, window, cx);
        });

        if let Some(active_item) = self.active_item() {
            if self.focus_handle.is_focused(window) {
                // Schedule a redraw next frame, so that the focus changes below take effect
                cx.on_next_frame(window, |_, _, cx| {
                    cx.notify();
                });

                // Pane was focused directly. We need to either focus a view inside the active item,
                // or focus the active item itself
                if let Some(weak_last_focus_handle) =
                    self.last_focus_handle_by_item.get(&active_item.item_id())
                {
                    if let Some(focus_handle) = weak_last_focus_handle.upgrade() {
                        focus_handle.focus(window);
                        return;
                    }
                }

                active_item.item_focus_handle(cx).focus(window);
            } else if let Some(focused) = window.focused(cx) {
                if !self.context_menu_focused(window, cx) {
                    self.last_focus_handle_by_item
                        .insert(active_item.item_id(), focused.downgrade());
                }
            }
        }
    }

    pub fn context_menu_focused(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.new_item_context_menu_handle.is_focused(window, cx)
            || self.split_item_context_menu_handle.is_focused(window, cx)
    }

    fn focus_out(&mut self, _event: FocusOutEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.was_focused = false;
        self.toolbar.update(cx, |toolbar, cx| {
            toolbar.focus_changed(false, window, cx);
        });
        cx.notify();
    }

    fn project_events(
        &mut self,
        _project: Entity<Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            project::Event::DiskBasedDiagnosticsFinished { .. }
            | project::Event::DiagnosticsUpdated { .. } => {
                if ItemSettings::get_global(cx).show_diagnostics != ShowDiagnostics::Off {
                    self.update_diagnostics(cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn update_diagnostics(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let show_diagnostics = ItemSettings::get_global(cx).show_diagnostics;
        self.diagnostics = if show_diagnostics != ShowDiagnostics::Off {
            project
                .read(cx)
                .diagnostic_summaries(false, cx)
                .filter_map(|(project_path, _, diagnostic_summary)| {
                    if diagnostic_summary.error_count > 0 {
                        Some((project_path, DiagnosticSeverity::ERROR))
                    } else if diagnostic_summary.warning_count > 0
                        && show_diagnostics != ShowDiagnostics::Errors
                    {
                        Some((project_path, DiagnosticSeverity::WARNING))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            HashMap::default()
        }
    }

    fn settings_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab_bar_settings = TabBarSettings::get_global(cx);
        let new_max_tabs = WorkspaceSettings::get_global(cx).max_tabs;

        if let Some(display_nav_history_buttons) = self.display_nav_history_buttons.as_mut() {
            *display_nav_history_buttons = tab_bar_settings.show_nav_history_buttons;
        }

        self.show_tab_bar_buttons = tab_bar_settings.show_tab_bar_buttons;

        if !PreviewTabsSettings::get_global(cx).enabled {
            self.preview_item_id = None;
        }

        if new_max_tabs != self.max_tabs {
            self.max_tabs = new_max_tabs;
            self.close_items_on_settings_change(window, cx);
        }

        self.update_diagnostics(cx);
        cx.notify();
    }

    pub fn active_item_index(&self) -> usize {
        self.active_item_index
    }

    pub fn activation_history(&self) -> &[ActivationHistoryEntry] {
        &self.activation_history
    }

    pub fn set_should_display_tab_bar<F>(&mut self, should_display_tab_bar: F)
    where
        F: 'static + Fn(&Window, &mut Context<Pane>) -> bool,
    {
        self.should_display_tab_bar = Rc::new(should_display_tab_bar);
    }

    pub fn set_can_split(
        &mut self,
        can_split_predicate: Option<
            Arc<dyn Fn(&mut Self, &dyn Any, &mut Window, &mut Context<Self>) -> bool + 'static>,
        >,
    ) {
        self.can_split_predicate = can_split_predicate;
    }

    pub fn set_can_toggle_zoom(&mut self, can_toggle_zoom: bool, cx: &mut Context<Self>) {
        self.can_toggle_zoom = can_toggle_zoom;
        cx.notify();
    }

    pub fn set_close_pane_if_empty(&mut self, close_pane_if_empty: bool, cx: &mut Context<Self>) {
        self.close_pane_if_empty = close_pane_if_empty;
        cx.notify();
    }

    pub fn set_can_navigate(&mut self, can_navigate: bool, cx: &mut Context<Self>) {
        self.toolbar.update(cx, |toolbar, cx| {
            toolbar.set_can_navigate(can_navigate, cx);
        });
        cx.notify();
    }

    pub fn set_render_tab_bar<F>(&mut self, cx: &mut Context<Self>, render: F)
    where
        F: 'static + Fn(&mut Pane, &mut Window, &mut Context<Pane>) -> AnyElement,
    {
        self.render_tab_bar = Rc::new(render);
        cx.notify();
    }

    pub fn set_render_tab_bar_buttons<F>(&mut self, cx: &mut Context<Self>, render: F)
    where
        F: 'static
            + Fn(
                &mut Pane,
                &mut Window,
                &mut Context<Pane>,
            ) -> (Option<AnyElement>, Option<AnyElement>),
    {
        self.render_tab_bar_buttons = Rc::new(render);
        cx.notify();
    }

    pub fn set_custom_drop_handle<F>(&mut self, cx: &mut Context<Self>, handle: F)
    where
        F: 'static
            + Fn(&mut Pane, &dyn Any, &mut Window, &mut Context<Pane>) -> ControlFlow<(), ()>,
    {
        self.custom_drop_handle = Some(Arc::new(handle));
        cx.notify();
    }

    pub fn nav_history_for_item<T: Item>(&self, item: &Entity<T>) -> ItemNavHistory {
        ItemNavHistory {
            history: self.nav_history.clone(),
            item: Arc::new(item.downgrade()),
            is_preview: self.preview_item_id == Some(item.item_id()),
        }
    }

    pub fn nav_history(&self) -> &NavHistory {
        &self.nav_history
    }

    pub fn nav_history_mut(&mut self) -> &mut NavHistory {
        &mut self.nav_history
    }

    pub fn disable_history(&mut self) {
        self.nav_history.disable();
    }

    pub fn enable_history(&mut self) {
        self.nav_history.enable();
    }

    pub fn can_navigate_backward(&self) -> bool {
        !self.nav_history.0.lock().backward_stack.is_empty()
    }

    pub fn can_navigate_forward(&self) -> bool {
        !self.nav_history.0.lock().forward_stack.is_empty()
    }

    pub fn navigate_backward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            let pane = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.go_back(pane, window, cx).detach_and_log_err(cx)
                })
            })
        }
    }

    fn navigate_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            let pane = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .go_forward(pane, window, cx)
                        .detach_and_log_err(cx)
                })
            })
        }
    }

    fn history_updated(&mut self, cx: &mut Context<Self>) {
        self.toolbar.update(cx, |_, cx| cx.notify());
    }

    pub fn preview_item_id(&self) -> Option<EntityId> {
        self.preview_item_id
    }

    pub fn preview_item(&self) -> Option<Box<dyn ItemHandle>> {
        self.preview_item_id
            .and_then(|id| self.items.iter().find(|item| item.item_id() == id))
            .cloned()
    }

    pub fn preview_item_idx(&self) -> Option<usize> {
        if let Some(preview_item_id) = self.preview_item_id {
            self.items
                .iter()
                .position(|item| item.item_id() == preview_item_id)
        } else {
            None
        }
    }

    pub fn is_active_preview_item(&self, item_id: EntityId) -> bool {
        self.preview_item_id == Some(item_id)
    }

    /// Marks the item with the given ID as the preview item.
    /// This will be ignored if the global setting `preview_tabs` is disabled.
    pub fn set_preview_item_id(&mut self, item_id: Option<EntityId>, cx: &App) {
        if PreviewTabsSettings::get_global(cx).enabled {
            self.preview_item_id = item_id;
        }
    }

    /// Should only be used when deserializing a pane.
    pub fn set_pinned_count(&mut self, count: usize) {
        self.pinned_tab_count = count;
    }

    pub fn pinned_count(&self) -> usize {
        self.pinned_tab_count
    }

    pub fn handle_item_edit(&mut self, item_id: EntityId, cx: &App) {
        if let Some(preview_item) = self.preview_item() {
            if preview_item.item_id() == item_id && !preview_item.preserve_preview(cx) {
                self.set_preview_item_id(None, cx);
            }
        }
    }

    pub(crate) fn open_item(
        &mut self,
        project_entry_id: Option<ProjectEntryId>,
        project_path: ProjectPath,
        focus_item: bool,
        allow_preview: bool,
        activate: bool,
        suggested_position: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build_item: WorkspaceItemBuilder,
    ) -> Box<dyn ItemHandle> {
        let mut existing_item = None;
        if let Some(project_entry_id) = project_entry_id {
            for (index, item) in self.items.iter().enumerate() {
                if item.is_singleton(cx)
                    && item.project_entry_ids(cx).as_slice() == [project_entry_id]
                {
                    let item = item.boxed_clone();
                    existing_item = Some((index, item));
                    break;
                }
            }
        } else {
            for (index, item) in self.items.iter().enumerate() {
                if item.is_singleton(cx) && item.project_path(cx).as_ref() == Some(&project_path) {
                    let item = item.boxed_clone();
                    existing_item = Some((index, item));
                    break;
                }
            }
        }
        if let Some((index, existing_item)) = existing_item {
            // If the item is already open, and the item is a preview item
            // and we are not allowing items to open as preview, mark the item as persistent.
            if let Some(preview_item_id) = self.preview_item_id {
                if let Some(tab) = self.items.get(index) {
                    if tab.item_id() == preview_item_id && !allow_preview {
                        self.set_preview_item_id(None, cx);
                    }
                }
            }
            if activate {
                self.activate_item(index, focus_item, focus_item, window, cx);
            }
            existing_item
        } else {
            // If the item is being opened as preview and we have an existing preview tab,
            // open the new item in the position of the existing preview tab.
            let destination_index = if allow_preview {
                self.close_current_preview_item(window, cx)
            } else {
                suggested_position
            };

            let new_item = build_item(self, window, cx);

            if allow_preview {
                self.set_preview_item_id(Some(new_item.item_id()), cx);
            }
            self.add_item_inner(
                new_item.clone(),
                true,
                focus_item,
                activate,
                destination_index,
                window,
                cx,
            );

            new_item
        }
    }

    pub fn close_current_preview_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let item_idx = self.preview_item_idx()?;
        let id = self.preview_item_id()?;

        let prev_active_item_index = self.active_item_index;
        self.remove_item(id, false, false, window, cx);
        self.active_item_index = prev_active_item_index;

        if item_idx < self.items.len() {
            Some(item_idx)
        } else {
            None
        }
    }

    pub fn add_item_inner(
        &mut self,
        item: Box<dyn ItemHandle>,
        activate_pane: bool,
        focus_item: bool,
        activate: bool,
        destination_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let item_already_exists = self
            .items
            .iter()
            .any(|existing_item| existing_item.item_id() == item.item_id());

        if !item_already_exists {
            self.close_items_on_item_open(window, cx);
        }

        if item.is_singleton(cx) {
            if let Some(&entry_id) = item.project_entry_ids(cx).first() {
                let Some(project) = self.project.upgrade() else {
                    return;
                };

                let project = project.read(cx);
                if let Some(project_path) = project.path_for_entry(entry_id, cx) {
                    let abs_path = project.absolute_path(&project_path, cx);
                    self.nav_history
                        .0
                        .lock()
                        .paths_by_item
                        .insert(item.item_id(), (project_path, abs_path));
                }
            }
        }
        // If no destination index is specified, add or move the item after the
        // active item (or at the start of tab bar, if the active item is pinned)
        let mut insertion_index = {
            cmp::min(
                if let Some(destination_index) = destination_index {
                    destination_index
                } else {
                    cmp::max(self.active_item_index + 1, self.pinned_count())
                },
                self.items.len(),
            )
        };

        // Does the item already exist?
        let project_entry_id = if item.is_singleton(cx) {
            item.project_entry_ids(cx).first().copied()
        } else {
            None
        };

        let existing_item_index = self.items.iter().position(|existing_item| {
            if existing_item.item_id() == item.item_id() {
                true
            } else if existing_item.is_singleton(cx) {
                existing_item
                    .project_entry_ids(cx)
                    .first()
                    .map_or(false, |existing_entry_id| {
                        Some(existing_entry_id) == project_entry_id.as_ref()
                    })
            } else {
                false
            }
        });

        if let Some(existing_item_index) = existing_item_index {
            // If the item already exists, move it to the desired destination and activate it

            if existing_item_index != insertion_index {
                let existing_item_is_active = existing_item_index == self.active_item_index;

                // If the caller didn't specify a destination and the added item is already
                // the active one, don't move it
                if existing_item_is_active && destination_index.is_none() {
                    insertion_index = existing_item_index;
                } else {
                    self.items.remove(existing_item_index);
                    if existing_item_index < self.active_item_index {
                        self.active_item_index -= 1;
                    }
                    insertion_index = insertion_index.min(self.items.len());

                    self.items.insert(insertion_index, item.clone());

                    if existing_item_is_active {
                        self.active_item_index = insertion_index;
                    } else if insertion_index <= self.active_item_index {
                        self.active_item_index += 1;
                    }
                }

                cx.notify();
            }

            if activate {
                self.activate_item(insertion_index, activate_pane, focus_item, window, cx);
            }
        } else {
            self.items.insert(insertion_index, item.clone());

            if activate {
                if insertion_index <= self.active_item_index
                    && self.preview_item_idx() != Some(self.active_item_index)
                {
                    self.active_item_index += 1;
                }

                self.activate_item(insertion_index, activate_pane, focus_item, window, cx);
            }
            cx.notify();
        }

        cx.emit(Event::AddItem { item });
    }

    pub fn add_item(
        &mut self,
        item: Box<dyn ItemHandle>,
        activate_pane: bool,
        focus_item: bool,
        destination_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_item_inner(
            item,
            activate_pane,
            focus_item,
            true,
            destination_index,
            window,
            cx,
        )
    }

    pub fn items_len(&self) -> usize {
        self.items.len()
    }

    pub fn items(&self) -> impl DoubleEndedIterator<Item = &Box<dyn ItemHandle>> {
        self.items.iter()
    }

    pub fn items_of_type<T: Render>(&self) -> impl '_ + Iterator<Item = Entity<T>> {
        self.items
            .iter()
            .filter_map(|item| item.to_any().downcast().ok())
    }

    pub fn active_item(&self) -> Option<Box<dyn ItemHandle>> {
        self.items.get(self.active_item_index).cloned()
    }

    fn active_item_id(&self) -> EntityId {
        self.items[self.active_item_index].item_id()
    }

    pub fn pixel_position_of_cursor(&self, cx: &App) -> Option<Point<Pixels>> {
        self.items
            .get(self.active_item_index)?
            .pixel_position_of_cursor(cx)
    }

    pub fn item_for_entry(
        &self,
        entry_id: ProjectEntryId,
        cx: &App,
    ) -> Option<Box<dyn ItemHandle>> {
        self.items.iter().find_map(|item| {
            if item.is_singleton(cx) && (item.project_entry_ids(cx).as_slice() == [entry_id]) {
                Some(item.boxed_clone())
            } else {
                None
            }
        })
    }

    pub fn item_for_path(
        &self,
        project_path: ProjectPath,
        cx: &App,
    ) -> Option<Box<dyn ItemHandle>> {
        self.items.iter().find_map(move |item| {
            if item.is_singleton(cx) && (item.project_path(cx).as_slice() == [project_path.clone()])
            {
                Some(item.boxed_clone())
            } else {
                None
            }
        })
    }

    pub fn index_for_item(&self, item: &dyn ItemHandle) -> Option<usize> {
        self.index_for_item_id(item.item_id())
    }

    fn index_for_item_id(&self, item_id: EntityId) -> Option<usize> {
        self.items.iter().position(|i| i.item_id() == item_id)
    }

    pub fn item_for_index(&self, ix: usize) -> Option<&dyn ItemHandle> {
        self.items.get(ix).map(|i| i.as_ref())
    }

    pub fn toggle_zoom(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_toggle_zoom {
            cx.propagate();
        } else if self.zoomed {
            cx.emit(Event::ZoomOut);
        } else if !self.items.is_empty() {
            if !self.focus_handle.contains_focused(window, cx) {
                cx.focus_self(window);
            }
            cx.emit(Event::ZoomIn);
        }
    }

    pub fn activate_item(
        &mut self,
        index: usize,
        activate_pane: bool,
        focus_item: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use NavigationMode::{GoingBack, GoingForward};
        if index < self.items.len() {
            let prev_active_item_ix = mem::replace(&mut self.active_item_index, index);
            if prev_active_item_ix != self.active_item_index
                || matches!(self.nav_history.mode(), GoingBack | GoingForward)
            {
                if let Some(prev_item) = self.items.get(prev_active_item_ix) {
                    prev_item.deactivated(window, cx);
                }
            }
            self.update_history(index);
            self.update_toolbar(window, cx);
            self.update_status_bar(window, cx);

            if focus_item {
                self.focus_active_item(window, cx);
            }

            cx.emit(Event::ActivateItem {
                local: activate_pane,
                focus_changed: focus_item,
            });

            if !self.is_tab_pinned(index) {
                self.tab_bar_scroll_handle
                    .scroll_to_item(index - self.pinned_tab_count);
            }

            cx.notify();
        }
    }

    fn update_history(&mut self, index: usize) {
        if let Some(newly_active_item) = self.items.get(index) {
            self.activation_history
                .retain(|entry| entry.entity_id != newly_active_item.item_id());
            self.activation_history.push(ActivationHistoryEntry {
                entity_id: newly_active_item.item_id(),
                timestamp: self
                    .next_activation_timestamp
                    .fetch_add(1, Ordering::SeqCst),
            });
        }
    }

    pub fn activate_prev_item(
        &mut self,
        activate_pane: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut index = self.active_item_index;
        if index > 0 {
            index -= 1;
        } else if !self.items.is_empty() {
            index = self.items.len() - 1;
        }
        self.activate_item(index, activate_pane, activate_pane, window, cx);
    }

    pub fn activate_next_item(
        &mut self,
        activate_pane: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut index = self.active_item_index;
        if index + 1 < self.items.len() {
            index += 1;
        } else {
            index = 0;
        }
        self.activate_item(index, activate_pane, activate_pane, window, cx);
    }

    pub fn swap_item_left(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.active_item_index;
        if index == 0 {
            return;
        }

        self.items.swap(index, index - 1);
        self.activate_item(index - 1, true, true, window, cx);
    }

    pub fn swap_item_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.active_item_index;
        if index + 1 == self.items.len() {
            return;
        }

        self.items.swap(index, index + 1);
        self.activate_item(index + 1, true, true, window, cx);
    }

    pub fn close_active_item(
        &mut self,
        action: &CloseActiveItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.items.is_empty() {
            // Close the window when there's no active items to close, if configured
            if WorkspaceSettings::get_global(cx)
                .when_closing_with_no_tabs
                .should_close()
            {
                window.dispatch_action(Box::new(CloseWindow), cx);
            }

            return Task::ready(Ok(()));
        }
        if self.is_tab_pinned(self.active_item_index) && !action.close_pinned {
            // Activate any non-pinned tab in same pane
            let non_pinned_tab_index = self
                .items()
                .enumerate()
                .find(|(index, _item)| !self.is_tab_pinned(*index))
                .map(|(index, _item)| index);
            if let Some(index) = non_pinned_tab_index {
                self.activate_item(index, false, false, window, cx);
                return Task::ready(Ok(()));
            }

            // Activate any non-pinned tab in different pane
            let current_pane = cx.entity();
            self.workspace
                .update(cx, |workspace, cx| {
                    let panes = workspace.center.panes();
                    let pane_with_unpinned_tab = panes.iter().find(|pane| {
                        if **pane == &current_pane {
                            return false;
                        }
                        pane.read(cx).has_unpinned_tabs()
                    });
                    if let Some(pane) = pane_with_unpinned_tab {
                        pane.update(cx, |pane, cx| pane.activate_unpinned_tab(window, cx));
                    }
                })
                .ok();

            return Task::ready(Ok(()));
        };

        let active_item_id = self.active_item_id();

        self.close_item_by_id(
            active_item_id,
            action.save_intent.unwrap_or(SaveIntent::Close),
            window,
            cx,
        )
    }

    pub fn close_item_by_id(
        &mut self,
        item_id_to_close: EntityId,
        save_intent: SaveIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.close_items(window, cx, save_intent, move |view_id| {
            view_id == item_id_to_close
        })
    }

    pub fn close_inactive_items(
        &mut self,
        action: &CloseInactiveItems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.items.is_empty() {
            return Task::ready(Ok(()));
        }

        let active_item_id = self.active_item_id();
        let pinned_item_ids = self.pinned_item_ids();

        self.close_items(
            window,
            cx,
            action.save_intent.unwrap_or(SaveIntent::Close),
            move |item_id| {
                item_id != active_item_id
                    && (action.close_pinned || !pinned_item_ids.contains(&item_id))
            },
        )
    }

    pub fn close_clean_items(
        &mut self,
        action: &CloseCleanItems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.items.is_empty() {
            return Task::ready(Ok(()));
        }

        let clean_item_ids = self.clean_item_ids(cx);
        let pinned_item_ids = self.pinned_item_ids();

        self.close_items(window, cx, SaveIntent::Close, move |item_id| {
            clean_item_ids.contains(&item_id)
                && (action.close_pinned || !pinned_item_ids.contains(&item_id))
        })
    }

    pub fn close_items_to_the_left_by_id(
        &mut self,
        item_id: Option<EntityId>,
        action: &CloseItemsToTheLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.close_items_to_the_side_by_id(item_id, Side::Left, action.close_pinned, window, cx)
    }

    pub fn close_items_to_the_right_by_id(
        &mut self,
        item_id: Option<EntityId>,
        action: &CloseItemsToTheRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.close_items_to_the_side_by_id(item_id, Side::Right, action.close_pinned, window, cx)
    }

    pub fn close_items_to_the_side_by_id(
        &mut self,
        item_id: Option<EntityId>,
        side: Side,
        close_pinned: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.items.is_empty() {
            return Task::ready(Ok(()));
        }

        let item_id = item_id.unwrap_or_else(|| self.active_item_id());
        let to_the_side_item_ids = self.to_the_side_item_ids(item_id, side);
        let pinned_item_ids = self.pinned_item_ids();

        self.close_items(window, cx, SaveIntent::Close, move |item_id| {
            to_the_side_item_ids.contains(&item_id)
                && (close_pinned || !pinned_item_ids.contains(&item_id))
        })
    }

    pub fn close_all_items(
        &mut self,
        action: &CloseAllItems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.items.is_empty() {
            return Task::ready(Ok(()));
        }

        let pinned_item_ids = self.pinned_item_ids();

        self.close_items(
            window,
            cx,
            action.save_intent.unwrap_or(SaveIntent::Close),
            |item_id| action.close_pinned || !pinned_item_ids.contains(&item_id),
        )
    }

    fn close_items_on_item_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.max_tabs.map(|m| m.get());
        let protect_active_item = false;
        self.close_items_to_target_count(target, protect_active_item, window, cx);
    }

    fn close_items_on_settings_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.max_tabs.map(|m| m.get() + 1);
        // The active item in this case is the settings.json file, which should be protected from being closed
        let protect_active_item = true;
        self.close_items_to_target_count(target, protect_active_item, window, cx);
    }

    fn close_items_to_target_count(
        &mut self,
        target_count: Option<usize>,
        protect_active_item: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target_count) = target_count else {
            return;
        };

        let mut index_list = Vec::new();
        let mut items_len = self.items_len();
        let mut indexes: HashMap<EntityId, usize> = HashMap::default();
        let active_ix = self.active_item_index();

        for (index, item) in self.items.iter().enumerate() {
            indexes.insert(item.item_id(), index);
        }

        // Close least recently used items to reach target count.
        // The target count is allowed to be exceeded, as we protect pinned
        // items, dirty items, and sometimes, the active item.
        for entry in self.activation_history.iter() {
            if items_len < target_count {
                break;
            }

            let Some(&index) = indexes.get(&entry.entity_id) else {
                continue;
            };

            if protect_active_item && index == active_ix {
                continue;
            }

            if let Some(true) = self.items.get(index).map(|item| item.is_dirty(cx)) {
                continue;
            }

            if self.is_tab_pinned(index) {
                continue;
            }

            index_list.push(index);
            items_len -= 1;
        }
        // The sort and reverse is necessary since we remove items
        // using their index position, hence removing from the end
        // of the list first to avoid changing indexes.
        index_list.sort_unstable();
        index_list
            .iter()
            .rev()
            .for_each(|&index| self._remove_item(index, false, false, None, window, cx));
    }

    // Usually when you close an item that has unsaved changes, we prompt you to
    // save it. That said, if you still have the buffer open in a different pane
    // we can close this one without fear of losing data.
    pub fn skip_save_on_close(item: &dyn ItemHandle, workspace: &Workspace, cx: &App) -> bool {
        let mut dirty_project_item_ids = Vec::new();
        item.for_each_project_item(cx, &mut |project_item_id, project_item| {
            if project_item.is_dirty() {
                dirty_project_item_ids.push(project_item_id);
            }
        });
        if dirty_project_item_ids.is_empty() {
            return !(item.is_singleton(cx) && item.is_dirty(cx));
        }

        for open_item in workspace.items(cx) {
            if open_item.item_id() == item.item_id() {
                continue;
            }
            if !open_item.is_singleton(cx) {
                continue;
            }
            let other_project_item_ids = open_item.project_item_model_ids(cx);
            dirty_project_item_ids.retain(|id| !other_project_item_ids.contains(id));
        }
        return dirty_project_item_ids.is_empty();
    }

    pub(super) fn file_names_for_prompt(
        items: &mut dyn Iterator<Item = &Box<dyn ItemHandle>>,
        cx: &App,
    ) -> String {
        let mut file_names = BTreeSet::default();
        for item in items {
            item.for_each_project_item(cx, &mut |_, project_item| {
                if !project_item.is_dirty() {
                    return;
                }
                let filename = project_item.project_path(cx).and_then(|path| {
                    path.path
                        .file_name()
                        .and_then(|name| name.to_str().map(ToOwned::to_owned))
                });
                file_names.insert(filename.unwrap_or("untitled".to_string()));
            });
        }
        if file_names.len() > 6 {
            format!(
                "{}\n.. and {} more",
                file_names.iter().take(5).join("\n"),
                file_names.len() - 5
            )
        } else {
            file_names.into_iter().join("\n")
        }
    }

    pub fn close_items(
        &self,
        window: &mut Window,
        cx: &mut Context<Pane>,
        mut save_intent: SaveIntent,
        should_close: impl Fn(EntityId) -> bool,
    ) -> Task<Result<()>> {
        // Find the items to close.
        let mut items_to_close = Vec::new();
        for item in &self.items {
            if should_close(item.item_id()) {
                items_to_close.push(item.boxed_clone());
            }
        }

        let active_item_id = self.active_item().map(|item| item.item_id());

        items_to_close.sort_by_key(|item| {
            let path = item.project_path(cx);
            // Put the currently active item at the end, because if the currently active item is not closed last
            // closing the currently active item will cause the focus to switch to another item
            // This will cause Zed to expand the content of the currently active item
            //
            // Beyond that sort in order of project path, with untitled files and multibuffers coming last.
            (active_item_id == Some(item.item_id()), path.is_none(), path)
        });

        let workspace = self.workspace.clone();
        let Some(project) = self.project.upgrade() else {
            return Task::ready(Ok(()));
        };
        cx.spawn_in(window, async move |pane, cx| {
            let dirty_items = workspace.update(cx, |workspace, cx| {
                items_to_close
                    .iter()
                    .filter(|item| {
                        item.is_dirty(cx)
                            && !Self::skip_save_on_close(item.as_ref(), &workspace, cx)
                    })
                    .map(|item| item.boxed_clone())
                    .collect::<Vec<_>>()
            })?;

            if save_intent == SaveIntent::Close && dirty_items.len() > 1 {
                let answer = pane.update_in(cx, |_, window, cx| {
                    let detail = Self::file_names_for_prompt(&mut dirty_items.iter(), cx);
                    window.prompt(
                        PromptLevel::Warning,
                        "Do you want to save changes to the following files?",
                        Some(&detail),
                        &["Save all", "Discard all", "Cancel"],
                        cx,
                    )
                })?;
                match answer.await {
                    Ok(0) => save_intent = SaveIntent::SaveAll,
                    Ok(1) => save_intent = SaveIntent::Skip,
                    Ok(2) => return Ok(()),
                    _ => {}
                }
            }

            for item_to_close in items_to_close {
                let mut should_save = true;
                if save_intent == SaveIntent::Close {
                    workspace.update(cx, |workspace, cx| {
                        if Self::skip_save_on_close(item_to_close.as_ref(), &workspace, cx) {
                            should_save = false;
                        }
                    })?;
                }

                if should_save {
                    if !Self::save_item(project.clone(), &pane, &*item_to_close, save_intent, cx)
                        .await?
                    {
                        break;
                    }
                }

                // Remove the item from the pane.
                pane.update_in(cx, |pane, window, cx| {
                    pane.remove_item(
                        item_to_close.item_id(),
                        false,
                        pane.close_pane_if_empty,
                        window,
                        cx,
                    );
                })
                .ok();
            }

            pane.update(cx, |_, cx| cx.notify()).ok();
            Ok(())
        })
    }

    pub fn remove_item(
        &mut self,
        item_id: EntityId,
        activate_pane: bool,
        close_pane_if_empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item_index) = self.index_for_item_id(item_id) else {
            return;
        };
        self._remove_item(
            item_index,
            activate_pane,
            close_pane_if_empty,
            None,
            window,
            cx,
        )
    }

    pub fn remove_item_and_focus_on_pane(
        &mut self,
        item_index: usize,
        activate_pane: bool,
        focus_on_pane_if_closed: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._remove_item(
            item_index,
            activate_pane,
            true,
            Some(focus_on_pane_if_closed),
            window,
            cx,
        )
    }

    fn _remove_item(
        &mut self,
        item_index: usize,
        activate_pane: bool,
        close_pane_if_empty: bool,
        focus_on_pane_if_closed: Option<Entity<Pane>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let activate_on_close = &ItemSettings::get_global(cx).activate_on_close;
        self.activation_history
            .retain(|entry| entry.entity_id != self.items[item_index].item_id());

        if self.is_tab_pinned(item_index) {
            self.pinned_tab_count -= 1;
        }
        if item_index == self.active_item_index {
            let left_neighbour_index = || item_index.min(self.items.len()).saturating_sub(1);
            let index_to_activate = match activate_on_close {
                ActivateOnClose::History => self
                    .activation_history
                    .pop()
                    .and_then(|last_activated_item| {
                        self.items.iter().enumerate().find_map(|(index, item)| {
                            (item.item_id() == last_activated_item.entity_id).then_some(index)
                        })
                    })
                    // We didn't have a valid activation history entry, so fallback
                    // to activating the item to the left
                    .unwrap_or_else(left_neighbour_index),
                ActivateOnClose::Neighbour => {
                    self.activation_history.pop();
                    if item_index + 1 < self.items.len() {
                        item_index + 1
                    } else {
                        item_index.saturating_sub(1)
                    }
                }
                ActivateOnClose::LeftNeighbour => {
                    self.activation_history.pop();
                    left_neighbour_index()
                }
            };

            let should_activate = activate_pane || self.has_focus(window, cx);
            if self.items.len() == 1 && should_activate {
                self.focus_handle.focus(window);
            } else {
                self.activate_item(
                    index_to_activate,
                    should_activate,
                    should_activate,
                    window,
                    cx,
                );
            }
        }

        let item = self.items.remove(item_index);

        cx.emit(Event::RemovedItem { item: item.clone() });
        if self.items.is_empty() {
            item.deactivated(window, cx);
            if close_pane_if_empty {
                self.update_toolbar(window, cx);
                cx.emit(Event::Remove {
                    focus_on_pane: focus_on_pane_if_closed,
                });
            }
        }

        if item_index < self.active_item_index {
            self.active_item_index -= 1;
        }

        let mode = self.nav_history.mode();
        self.nav_history.set_mode(NavigationMode::ClosingItem);
        item.deactivated(window, cx);
        self.nav_history.set_mode(mode);

        if self.is_active_preview_item(item.item_id()) {
            self.set_preview_item_id(None, cx);
        }

        if let Some(path) = item.project_path(cx) {
            let abs_path = self
                .nav_history
                .0
                .lock()
                .paths_by_item
                .get(&item.item_id())
                .and_then(|(_, abs_path)| abs_path.clone());

            self.nav_history
                .0
                .lock()
                .paths_by_item
                .insert(item.item_id(), (path, abs_path));
        } else {
            self.nav_history
                .0
                .lock()
                .paths_by_item
                .remove(&item.item_id());
        }

        if self.zoom_out_on_close && self.items.is_empty() && close_pane_if_empty && self.zoomed {
            cx.emit(Event::ZoomOut);
        }

        cx.notify();
    }

    pub async fn save_item(
        project: Entity<Project>,
        pane: &WeakEntity<Pane>,
        item: &dyn ItemHandle,
        save_intent: SaveIntent,
        cx: &mut AsyncWindowContext,
    ) -> Result<bool> {
        const CONFLICT_MESSAGE: &str = "This file has changed on disk since you started editing it. Do you want to overwrite it?";

        const DELETED_MESSAGE: &str = "This file has been deleted on disk since you started editing it. Do you want to recreate it?";

        if save_intent == SaveIntent::Skip {
            return Ok(true);
        }
        let Some(item_ix) = pane
            .read_with(cx, |pane, _| pane.index_for_item(item))
            .ok()
            .flatten()
        else {
            return Ok(true);
        };

        let (
            mut has_conflict,
            mut is_dirty,
            mut can_save,
            can_save_as,
            is_singleton,
            has_deleted_file,
        ) = cx.update(|_window, cx| {
            (
                item.has_conflict(cx),
                item.is_dirty(cx),
                item.can_save(cx),
                item.can_save_as(cx),
                item.is_singleton(cx),
                item.has_deleted_file(cx),
            )
        })?;

        // when saving a single buffer, we ignore whether or not it's dirty.
        if save_intent == SaveIntent::Save || save_intent == SaveIntent::SaveWithoutFormat {
            is_dirty = true;
        }

        if save_intent == SaveIntent::SaveAs {
            is_dirty = true;
            has_conflict = false;
            can_save = false;
        }

        if save_intent == SaveIntent::Overwrite {
            has_conflict = false;
        }

        let should_format = save_intent != SaveIntent::SaveWithoutFormat;

        if has_conflict && can_save {
            if has_deleted_file && is_singleton {
                let answer = pane.update_in(cx, |pane, window, cx| {
                    pane.activate_item(item_ix, true, true, window, cx);
                    window.prompt(
                        PromptLevel::Warning,
                        DELETED_MESSAGE,
                        None,
                        &["Save", "Close", "Cancel"],
                        cx,
                    )
                })?;
                match answer.await {
                    Ok(0) => {
                        pane.update_in(cx, |_, window, cx| {
                            item.save(
                                SaveOptions {
                                    format: should_format,
                                    autosave: false,
                                },
                                project,
                                window,
                                cx,
                            )
                        })?
                        .await?
                    }
                    Ok(1) => {
                        pane.update_in(cx, |pane, window, cx| {
                            pane.remove_item(item.item_id(), false, true, window, cx)
                        })?;
                    }
                    _ => return Ok(false),
                }
                return Ok(true);
            } else {
                let answer = pane.update_in(cx, |pane, window, cx| {
                    pane.activate_item(item_ix, true, true, window, cx);
                    window.prompt(
                        PromptLevel::Warning,
                        CONFLICT_MESSAGE,
                        None,
                        &["Overwrite", "Discard", "Cancel"],
                        cx,
                    )
                })?;
                match answer.await {
                    Ok(0) => {
                        pane.update_in(cx, |_, window, cx| {
                            item.save(
                                SaveOptions {
                                    format: should_format,
                                    autosave: false,
                                },
                                project,
                                window,
                                cx,
                            )
                        })?
                        .await?
                    }
                    Ok(1) => {
                        pane.update_in(cx, |_, window, cx| item.reload(project, window, cx))?
                            .await?
                    }
                    _ => return Ok(false),
                }
            }
        } else if is_dirty && (can_save || can_save_as) {
            if save_intent == SaveIntent::Close {
                let will_autosave = cx.update(|_window, cx| {
                    matches!(
                        item.workspace_settings(cx).autosave,
                        AutosaveSetting::OnFocusChange | AutosaveSetting::OnWindowChange
                    ) && item.can_autosave(cx)
                })?;
                if !will_autosave {
                    let item_id = item.item_id();
                    let answer_task = pane.update_in(cx, |pane, window, cx| {
                        if pane.save_modals_spawned.insert(item_id) {
                            pane.activate_item(item_ix, true, true, window, cx);
                            let prompt = dirty_message_for(item.project_path(cx));
                            Some(window.prompt(
                                PromptLevel::Warning,
                                &prompt,
                                None,
                                &["Save", "Don't Save", "Cancel"],
                                cx,
                            ))
                        } else {
                            None
                        }
                    })?;
                    if let Some(answer_task) = answer_task {
                        let answer = answer_task.await;
                        pane.update(cx, |pane, _| {
                            if !pane.save_modals_spawned.remove(&item_id) {
                                debug_panic!(
                                    "save modal was not present in spawned modals after awaiting for its answer"
                                )
                            }
                        })?;
                        match answer {
                            Ok(0) => {}
                            Ok(1) => {
                                // Don't save this file
                                pane.update_in(cx, |pane, window, cx| {
                                    if pane.is_tab_pinned(item_ix) && !item.can_save(cx) {
                                        pane.pinned_tab_count -= 1;
                                    }
                                    item.discarded(project, window, cx)
                                })
                                .log_err();
                                return Ok(true);
                            }
                            _ => return Ok(false), // Cancel
                        }
                    } else {
                        return Ok(false);
                    }
                }
            }

            if can_save {
                pane.update_in(cx, |pane, window, cx| {
                    if pane.is_active_preview_item(item.item_id()) {
                        pane.set_preview_item_id(None, cx);
                    }
                    item.save(
                        SaveOptions {
                            format: should_format,
                            autosave: false,
                        },
                        project,
                        window,
                        cx,
                    )
                })?
                .await?;
            } else if can_save_as && is_singleton {
                let new_path = pane.update_in(cx, |pane, window, cx| {
                    pane.activate_item(item_ix, true, true, window, cx);
                    pane.workspace.update(cx, |workspace, cx| {
                        let lister = if workspace.project().read(cx).is_local() {
                            DirectoryLister::Local(
                                workspace.project().clone(),
                                workspace.app_state().fs.clone(),
                            )
                        } else {
                            DirectoryLister::Project(workspace.project().clone())
                        };
                        workspace.prompt_for_new_path(lister, window, cx)
                    })
                })??;
                let Some(new_path) = new_path.await.ok().flatten().into_iter().flatten().next()
                else {
                    return Ok(false);
                };

                let project_path = pane
                    .update(cx, |pane, cx| {
                        pane.project
                            .update(cx, |project, cx| {
                                project.find_or_create_worktree(new_path, true, cx)
                            })
                            .ok()
                    })
                    .ok()
                    .flatten();
                let save_task = if let Some(project_path) = project_path {
                    let (worktree, path) = project_path.await?;
                    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id())?;
                    let new_path = ProjectPath {
                        worktree_id,
                        path: path.into(),
                    };

                    pane.update_in(cx, |pane, window, cx| {
                        if let Some(item) = pane.item_for_path(new_path.clone(), cx) {
                            pane.remove_item(item.item_id(), false, false, window, cx);
                        }

                        item.save_as(project, new_path, window, cx)
                    })?
                } else {
                    return Ok(false);
                };

                save_task.await?;
                return Ok(true);
            }
        }

        pane.update(cx, |_, cx| {
            cx.emit(Event::UserSavedItem {
                item: item.downgrade_item(),
                save_intent,
            });
            true
        })
    }

    pub fn autosave_item(
        item: &dyn ItemHandle,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let format = !matches!(
            item.workspace_settings(cx).autosave,
            AutosaveSetting::AfterDelay { .. }
        );
        if item.can_autosave(cx) {
            item.save(
                SaveOptions {
                    format,
                    autosave: true,
                },
                project,
                window,
                cx,
            )
        } else {
            Task::ready(Ok(()))
        }
    }

    pub fn focus_active_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_item) = self.active_item() {
            let focus_handle = active_item.item_focus_handle(cx);
            window.focus(&focus_handle);
        }
    }

    pub fn split(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        cx.emit(Event::Split(direction));
    }

    pub fn toolbar(&self) -> &Entity<Toolbar> {
        &self.toolbar
    }

    pub fn handle_deleted_project_item(
        &mut self,
        entry_id: ProjectEntryId,
        window: &mut Window,
        cx: &mut Context<Pane>,
    ) -> Option<()> {
        let item_id = self.items().find_map(|item| {
            if item.is_singleton(cx) && item.project_entry_ids(cx).as_slice() == [entry_id] {
                Some(item.item_id())
            } else {
                None
            }
        })?;

        self.remove_item(item_id, false, true, window, cx);
        self.nav_history.remove_item(item_id);

        Some(())
    }

    fn update_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_item = self
            .items
            .get(self.active_item_index)
            .map(|item| item.as_ref());
        self.toolbar.update(cx, |toolbar, cx| {
            toolbar.set_active_item(active_item, window, cx);
        });
    }

    fn update_status_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let pane = cx.entity().clone();

        window.defer(cx, move |window, cx| {
            let Ok(status_bar) =
                workspace.read_with(cx, |workspace, _| workspace.status_bar.clone())
            else {
                return;
            };

            status_bar.update(cx, move |status_bar, cx| {
                status_bar.set_active_pane(&pane, window, cx);
            });
        });
    }

    fn entry_abs_path(&self, entry: ProjectEntryId, cx: &App) -> Option<PathBuf> {
        let worktree = self
            .workspace
            .upgrade()?
            .read(cx)
            .project()
            .read(cx)
            .worktree_for_entry(entry, cx)?
            .read(cx);
        let entry = worktree.entry_for_id(entry)?;
        match &entry.canonical_path {
            Some(canonical_path) => Some(canonical_path.to_path_buf()),
            None => worktree.absolutize(&entry.path).ok(),
        }
    }

    pub fn icon_color(selected: bool) -> Color {
        if selected {
            Color::Default
        } else {
            Color::Muted
        }
    }

    fn toggle_pin_tab(&mut self, _: &TogglePinTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let active_tab_ix = self.active_item_index();
        if self.is_tab_pinned(active_tab_ix) {
            self.unpin_tab_at(active_tab_ix, window, cx);
        } else {
            self.pin_tab_at(active_tab_ix, window, cx);
        }
    }

    fn unpin_all_tabs(&mut self, _: &UnpinAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }

        let pinned_item_ids = self.pinned_item_ids().into_iter().rev();

        for pinned_item_id in pinned_item_ids {
            if let Some(ix) = self.index_for_item_id(pinned_item_id) {
                self.unpin_tab_at(ix, window, cx);
            }
        }
    }

    fn pin_tab_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.change_tab_pin_state(ix, PinOperation::Pin, window, cx);
    }

    fn unpin_tab_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.change_tab_pin_state(ix, PinOperation::Unpin, window, cx);
    }

    fn change_tab_pin_state(
        &mut self,
        ix: usize,
        operation: PinOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        maybe!({
            let pane = cx.entity().clone();

            let destination_index = match operation {
                PinOperation::Pin => self.pinned_tab_count.min(ix),
                PinOperation::Unpin => self.pinned_tab_count.checked_sub(1)?,
            };

            let id = self.item_for_index(ix)?.item_id();
            let should_activate = ix == self.active_item_index;

            if matches!(operation, PinOperation::Pin) && self.is_active_preview_item(id) {
                self.set_preview_item_id(None, cx);
            }

            match operation {
                PinOperation::Pin => self.pinned_tab_count += 1,
                PinOperation::Unpin => self.pinned_tab_count -= 1,
            }

            if ix == destination_index {
                cx.notify();
            } else {
                self.workspace
                    .update(cx, |_, cx| {
                        cx.defer_in(window, move |_, window, cx| {
                            move_item(
                                &pane,
                                &pane,
                                id,
                                destination_index,
                                should_activate,
                                window,
                                cx,
                            );
                        });
                    })
                    .ok()?;
            }

            let event = match operation {
                PinOperation::Pin => Event::ItemPinned,
                PinOperation::Unpin => Event::ItemUnpinned,
            };

            cx.emit(event);

            Some(())
        });
    }

    fn is_tab_pinned(&self, ix: usize) -> bool {
        self.pinned_tab_count > ix
    }

    fn has_unpinned_tabs(&self) -> bool {
        self.pinned_tab_count < self.items.len()
    }

    fn activate_unpinned_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let Some(index) = self
            .items()
            .enumerate()
            .find_map(|(index, _item)| (!self.is_tab_pinned(index)).then_some(index))
        else {
            return;
        };
        self.activate_item(index, true, true, window, cx);
    }

    fn render_tab(
        &self,
        ix: usize,
        item: &dyn ItemHandle,
        detail: usize,
        focus_handle: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Pane>,
    ) -> impl IntoElement + use<> {
        let is_active = ix == self.active_item_index;
        let is_preview = self
            .preview_item_id
            .map(|id| id == item.item_id())
            .unwrap_or(false);

        let label = item.tab_content(
            TabContentParams {
                detail: Some(detail),
                selected: is_active,
                preview: is_preview,
                deemphasized: !self.has_focus(window, cx),
            },
            window,
            cx,
        );

        let item_diagnostic = item
            .project_path(cx)
            .map_or(None, |project_path| self.diagnostics.get(&project_path));

        let decorated_icon = item_diagnostic.map_or(None, |diagnostic| {
            let icon = match item.tab_icon(window, cx) {
                Some(icon) => icon,
                None => return None,
            };

            let knockout_item_color = if is_active {
                cx.theme().colors().tab_active_background
            } else {
                cx.theme().colors().tab_bar_background
            };

            let (icon_decoration, icon_color) = if matches!(diagnostic, &DiagnosticSeverity::ERROR)
            {
                (IconDecorationKind::X, Color::Error)
            } else {
                (IconDecorationKind::Triangle, Color::Warning)
            };

            Some(DecoratedIcon::new(
                icon.size(IconSize::Small).color(Color::Muted),
                Some(
                    IconDecoration::new(icon_decoration, knockout_item_color, cx)
                        .color(icon_color.color(cx))
                        .position(Point {
                            x: px(-2.),
                            y: px(-2.),
                        }),
                ),
            ))
        });

        let icon = if decorated_icon.is_none() {
            match item_diagnostic {
                Some(&DiagnosticSeverity::ERROR) => None,
                Some(&DiagnosticSeverity::WARNING) => None,
                _ => item
                    .tab_icon(window, cx)
                    .map(|icon| icon.color(Color::Muted)),
            }
            .map(|icon| icon.size(IconSize::Small))
        } else {
            None
        };

        let settings = ItemSettings::get_global(cx);
        let close_side = &settings.close_position;
        let show_close_button = &settings.show_close_button;
        let indicator = render_item_indicator(item.boxed_clone(), cx);
        let item_id = item.item_id();
        let is_first_item = ix == 0;
        let is_last_item = ix == self.items.len() - 1;
        let is_pinned = self.is_tab_pinned(ix);
        let position_relative_to_active_item = ix.cmp(&self.active_item_index);

        let tab = Tab::new(ix)
            .position(if is_first_item {
                TabPosition::First
            } else if is_last_item {
                TabPosition::Last
            } else {
                TabPosition::Middle(position_relative_to_active_item)
            })
            .close_side(match close_side {
                ClosePosition::Left => ui::TabCloseSide::Start,
                ClosePosition::Right => ui::TabCloseSide::End,
            })
            .toggle_state(is_active)
            .on_click(cx.listener(move |pane: &mut Self, _, window, cx| {
                pane.activate_item(ix, true, true, window, cx)
            }))
            // TODO: This should be a click listener with the middle mouse button instead of a mouse down listener.
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |pane, _event, window, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                        .detach_and_log_err(cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |pane, event: &MouseDownEvent, _, cx| {
                    if let Some(id) = pane.preview_item_id {
                        if id == item_id && event.click_count > 1 {
                            pane.set_preview_item_id(None, cx);
                        }
                    }
                }),
            )
            .on_drag(
                DraggedTab {
                    item: item.boxed_clone(),
                    pane: cx.entity().clone(),
                    detail,
                    is_active,
                    ix,
                },
                |tab, _, _, cx| cx.new(|_| tab.clone()),
            )
            .drag_over::<DraggedTab>(|tab, _, _, cx| {
                tab.bg(cx.theme().colors().drop_target_background)
            })
            .drag_over::<DraggedSelection>(|tab, _, _, cx| {
                tab.bg(cx.theme().colors().drop_target_background)
            })
            .when_some(self.can_drop_predicate.clone(), |this, p| {
                this.can_drop(move |a, window, cx| p(a, window, cx))
            })
            .on_drop(
                cx.listener(move |this, dragged_tab: &DraggedTab, window, cx| {
                    this.drag_split_direction = None;
                    this.handle_tab_drop(dragged_tab, ix, window, cx)
                }),
            )
            .on_drop(
                cx.listener(move |this, selection: &DraggedSelection, window, cx| {
                    this.drag_split_direction = None;
                    this.handle_dragged_selection_drop(selection, Some(ix), window, cx)
                }),
            )
            .on_drop(cx.listener(move |this, paths, window, cx| {
                this.drag_split_direction = None;
                this.handle_external_paths_drop(paths, window, cx)
            }))
            .when_some(item.tab_tooltip_content(cx), |tab, content| match content {
                TabTooltipContent::Text(text) => tab.tooltip(Tooltip::text(text.clone())),
                TabTooltipContent::Custom(element_fn) => {
                    tab.tooltip(move |window, cx| element_fn(window, cx))
                }
            })
            .start_slot::<Indicator>(indicator)
            .map(|this| {
                let end_slot_action: &'static dyn Action;
                let end_slot_tooltip_text: &'static str;
                let end_slot = if is_pinned {
                    end_slot_action = &TogglePinTab;
                    end_slot_tooltip_text = "Unpin Tab";
                    IconButton::new("unpin tab", IconName::Pin)
                        .shape(IconButtonShape::Square)
                        .icon_color(Color::Muted)
                        .size(ButtonSize::None)
                        .icon_size(IconSize::XSmall)
                        .on_click(cx.listener(move |pane, _, window, cx| {
                            pane.unpin_tab_at(ix, window, cx);
                        }))
                } else {
                    end_slot_action = &CloseActiveItem {
                        save_intent: None,
                        close_pinned: false,
                    };
                    end_slot_tooltip_text = "Close Tab";
                    match show_close_button {
                        ShowCloseButton::Always => IconButton::new("close tab", IconName::Close),
                        ShowCloseButton::Hover => {
                            IconButton::new("close tab", IconName::Close).visible_on_hover("")
                        }
                        ShowCloseButton::Hidden => return this,
                    }
                    .shape(IconButtonShape::Square)
                    .icon_color(Color::Muted)
                    .size(ButtonSize::None)
                    .icon_size(IconSize::XSmall)
                    .on_click(cx.listener(move |pane, _, window, cx| {
                        pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                            .detach_and_log_err(cx);
                    }))
                }
                .map(|this| {
                    if is_active {
                        let focus_handle = focus_handle.clone();
                        this.tooltip(move |window, cx| {
                            Tooltip::for_action_in(
                                end_slot_tooltip_text,
                                end_slot_action,
                                &focus_handle,
                                window,
                                cx,
                            )
                        })
                    } else {
                        this.tooltip(Tooltip::text(end_slot_tooltip_text))
                    }
                });
                this.end_slot(end_slot)
            })
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .children(
                        std::iter::once(if let Some(decorated_icon) = decorated_icon {
                            Some(div().child(decorated_icon.into_any_element()))
                        } else if let Some(icon) = icon {
                            Some(div().child(icon.into_any_element()))
                        } else {
                            None
                        })
                        .flatten(),
                    )
                    .child(label),
            );

        let single_entry_to_resolve = self.items[ix]
            .is_singleton(cx)
            .then(|| self.items[ix].project_entry_ids(cx).get(0).copied())
            .flatten();

        let total_items = self.items.len();
        let has_items_to_left = ix > 0;
        let has_items_to_right = ix < total_items - 1;
        let has_clean_items = self.items.iter().any(|item| !item.is_dirty(cx));
        let is_pinned = self.is_tab_pinned(ix);
        let pane = cx.entity().downgrade();
        let menu_context = item.item_focus_handle(cx);
        right_click_menu(ix)
            .trigger(|_| tab)
            .menu(move |window, cx| {
                let pane = pane.clone();
                let menu_context = menu_context.clone();
                ContextMenu::build(window, cx, move |mut menu, window, cx| {
                    let close_active_item_action = CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    };
                    let close_inactive_items_action = CloseInactiveItems {
                        save_intent: None,
                        close_pinned: false,
                    };
                    let close_items_to_the_left_action = CloseItemsToTheLeft {
                        close_pinned: false,
                    };
                    let close_items_to_the_right_action = CloseItemsToTheRight {
                        close_pinned: false,
                    };
                    let close_clean_items_action = CloseCleanItems {
                        close_pinned: false,
                    };
                    let close_all_items_action = CloseAllItems {
                        save_intent: None,
                        close_pinned: false,
                    };
                    if let Some(pane) = pane.upgrade() {
                        menu = menu
                            .entry(
                                "Close",
                                Some(Box::new(close_active_item_action)),
                                window.handler_for(&pane, move |pane, window, cx| {
                                    pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                                        .detach_and_log_err(cx);
                                }),
                            )
                            .item(ContextMenuItem::Entry(
                                ContextMenuEntry::new("Close Others")
                                    .action(Box::new(close_inactive_items_action.clone()))
                                    .disabled(total_items == 1)
                                    .handler(window.handler_for(&pane, move |pane, window, cx| {
                                        pane.close_inactive_items(
                                            &close_inactive_items_action,
                                            window,
                                            cx,
                                        )
                                        .detach_and_log_err(cx);
                                    })),
                            ))
                            .separator()
                            .item(ContextMenuItem::Entry(
                                ContextMenuEntry::new("Close Left")
                                    .action(Box::new(close_items_to_the_left_action.clone()))
                                    .disabled(!has_items_to_left)
                                    .handler(window.handler_for(&pane, move |pane, window, cx| {
                                        pane.close_items_to_the_left_by_id(
                                            Some(item_id),
                                            &close_items_to_the_left_action,
                                            window,
                                            cx,
                                        )
                                        .detach_and_log_err(cx);
                                    })),
                            ))
                            .item(ContextMenuItem::Entry(
                                ContextMenuEntry::new("Close Right")
                                    .action(Box::new(close_items_to_the_right_action.clone()))
                                    .disabled(!has_items_to_right)
                                    .handler(window.handler_for(&pane, move |pane, window, cx| {
                                        pane.close_items_to_the_right_by_id(
                                            Some(item_id),
                                            &close_items_to_the_right_action,
                                            window,
                                            cx,
                                        )
                                        .detach_and_log_err(cx);
                                    })),
                            ))
                            .separator()
                            .item(ContextMenuItem::Entry(
                                ContextMenuEntry::new("Close Clean")
                                    .action(Box::new(close_clean_items_action.clone()))
                                    .disabled(!has_clean_items)
                                    .handler(window.handler_for(&pane, move |pane, window, cx| {
                                        pane.close_clean_items(
                                            &close_clean_items_action,
                                            window,
                                            cx,
                                        )
                                        .detach_and_log_err(cx)
                                    })),
                            ))
                            .entry(
                                "Close All",
                                Some(Box::new(close_all_items_action.clone())),
                                window.handler_for(&pane, move |pane, window, cx| {
                                    pane.close_all_items(&close_all_items_action, window, cx)
                                        .detach_and_log_err(cx)
                                }),
                            );

                        let pin_tab_entries = |menu: ContextMenu| {
                            menu.separator().map(|this| {
                                if is_pinned {
                                    this.entry(
                                        "Unpin Tab",
                                        Some(TogglePinTab.boxed_clone()),
                                        window.handler_for(&pane, move |pane, window, cx| {
                                            pane.unpin_tab_at(ix, window, cx);
                                        }),
                                    )
                                } else {
                                    this.entry(
                                        "Pin Tab",
                                        Some(TogglePinTab.boxed_clone()),
                                        window.handler_for(&pane, move |pane, window, cx| {
                                            pane.pin_tab_at(ix, window, cx);
                                        }),
                                    )
                                }
                            })
                        };
                        if let Some(entry) = single_entry_to_resolve {
                            let project_path = pane
                                .read(cx)
                                .item_for_entry(entry, cx)
                                .and_then(|item| item.project_path(cx));
                            let worktree = project_path.as_ref().and_then(|project_path| {
                                pane.read(cx)
                                    .project
                                    .upgrade()?
                                    .read(cx)
                                    .worktree_for_id(project_path.worktree_id, cx)
                            });
                            let has_relative_path = worktree.as_ref().is_some_and(|worktree| {
                                worktree
                                    .read(cx)
                                    .root_entry()
                                    .map_or(false, |entry| entry.is_dir())
                            });

                            let entry_abs_path = pane.read(cx).entry_abs_path(entry, cx);
                            let parent_abs_path = entry_abs_path
                                .as_deref()
                                .and_then(|abs_path| Some(abs_path.parent()?.to_path_buf()));
                            let relative_path = project_path
                                .map(|project_path| project_path.path)
                                .filter(|_| has_relative_path);

                            let visible_in_project_panel = relative_path.is_some()
                                && worktree.is_some_and(|worktree| worktree.read(cx).is_visible());

                            let entry_id = entry.to_proto();
                            menu = menu
                                .separator()
                                .when_some(entry_abs_path, |menu, abs_path| {
                                    menu.entry(
                                        "Copy Path",
                                        Some(Box::new(zed_actions::workspace::CopyPath)),
                                        window.handler_for(&pane, move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                abs_path.to_string_lossy().to_string(),
                                            ));
                                        }),
                                    )
                                })
                                .when_some(relative_path, |menu, relative_path| {
                                    menu.entry(
                                        "Copy Relative Path",
                                        Some(Box::new(zed_actions::workspace::CopyRelativePath)),
                                        window.handler_for(&pane, move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                relative_path.to_string_lossy().to_string(),
                                            ));
                                        }),
                                    )
                                })
                                .map(pin_tab_entries)
                                .separator()
                                .when(visible_in_project_panel, |menu| {
                                    menu.entry(
                                        "Reveal In Project Panel",
                                        Some(Box::new(RevealInProjectPanel {
                                            entry_id: Some(entry_id),
                                        })),
                                        window.handler_for(&pane, move |pane, _, cx| {
                                            pane.project
                                                .update(cx, |_, cx| {
                                                    cx.emit(project::Event::RevealInProjectPanel(
                                                        ProjectEntryId::from_proto(entry_id),
                                                    ))
                                                })
                                                .ok();
                                        }),
                                    )
                                })
                                .when_some(parent_abs_path, |menu, parent_abs_path| {
                                    menu.entry(
                                        "Open in Terminal",
                                        Some(Box::new(OpenInTerminal)),
                                        window.handler_for(&pane, move |_, window, cx| {
                                            window.dispatch_action(
                                                OpenTerminal {
                                                    working_directory: parent_abs_path.clone(),
                                                }
                                                .boxed_clone(),
                                                cx,
                                            );
                                        }),
                                    )
                                });
                        } else {
                            menu = menu.map(pin_tab_entries);
                        }
                    }

                    menu.context(menu_context)
                })
            })
    }

    fn render_tab_bar(&mut self, window: &mut Window, cx: &mut Context<Pane>) -> AnyElement {
        let focus_handle = self.focus_handle.clone();
        let navigate_backward = IconButton::new("navigate_backward", IconName::ArrowLeft)
            .icon_size(IconSize::Small)
            .on_click({
                let entity = cx.entity().clone();
                move |_, window, cx| {
                    entity.update(cx, |pane, cx| pane.navigate_backward(window, cx))
                }
            })
            .disabled(!self.can_navigate_backward())
            .tooltip({
                let focus_handle = focus_handle.clone();
                move |window, cx| {
                    Tooltip::for_action_in("Go Back", &GoBack, &focus_handle, window, cx)
                }
            });

        let navigate_forward = IconButton::new("navigate_forward", IconName::ArrowRight)
            .icon_size(IconSize::Small)
            .on_click({
                let entity = cx.entity().clone();
                move |_, window, cx| entity.update(cx, |pane, cx| pane.navigate_forward(window, cx))
            })
            .disabled(!self.can_navigate_forward())
            .tooltip({
                let focus_handle = focus_handle.clone();
                move |window, cx| {
                    Tooltip::for_action_in("Go Forward", &GoForward, &focus_handle, window, cx)
                }
            });

        let mut tab_items = self
            .items
            .iter()
            .enumerate()
            .zip(tab_details(&self.items, window, cx))
            .map(|((ix, item), detail)| {
                self.render_tab(ix, &**item, detail, &focus_handle, window, cx)
            })
            .collect::<Vec<_>>();
        let tab_count = tab_items.len();
        if self.pinned_tab_count > tab_count {
            log::warn!(
                "Pinned tab count ({}) exceeds actual tab count ({}). \
                This should not happen. If possible, add reproduction steps, \
                in a comment, to https://github.com/zed-industries/zed/issues/33342",
                self.pinned_tab_count,
                tab_count
            );
            self.pinned_tab_count = tab_count;
        }
        let unpinned_tabs = tab_items.split_off(self.pinned_tab_count);
        let pinned_tabs = tab_items;
        TabBar::new("tab_bar")
            .when(
                self.display_nav_history_buttons.unwrap_or_default(),
                |tab_bar| {
                    tab_bar
                        .start_child(navigate_backward)
                        .start_child(navigate_forward)
                },
            )
            .map(|tab_bar| {
                if self.show_tab_bar_buttons {
                    let render_tab_buttons = self.render_tab_bar_buttons.clone();
                    let (left_children, right_children) = render_tab_buttons(self, window, cx);
                    tab_bar
                        .start_children(left_children)
                        .end_children(right_children)
                } else {
                    tab_bar
                }
            })
            .children(pinned_tabs.len().ne(&0).then(|| {
                let content_width = self.tab_bar_scroll_handle.content_size().width;
                let viewport_width = self.tab_bar_scroll_handle.viewport().size.width;
                // We need to check both because offset returns delta values even when the scroll handle is not scrollable
                let is_scrollable = content_width > viewport_width;
                let is_scrolled = self.tab_bar_scroll_handle.offset().x < px(0.);
                let has_active_unpinned_tab = self.active_item_index >= self.pinned_tab_count;
                h_flex()
                    .children(pinned_tabs)
                    .when(is_scrollable && is_scrolled, |this| {
                        this.when(has_active_unpinned_tab, |this| this.border_r_2())
                            .when(!has_active_unpinned_tab, |this| this.border_r_1())
                            .border_color(cx.theme().colors().border)
                    })
            }))
            .child(
                h_flex()
                    .id("unpinned tabs")
                    .overflow_x_scroll()
                    .w_full()
                    .track_scroll(&self.tab_bar_scroll_handle)
                    .children(unpinned_tabs)
                    .child(
                        div()
                            .id("tab_bar_drop_target")
                            .min_w_6()
                            // HACK: This empty child is currently necessary to force the drop target to appear
                            // despite us setting a min width above.
                            .child("")
                            .h_full()
                            .flex_grow()
                            .drag_over::<DraggedTab>(|bar, _, _, cx| {
                                bar.bg(cx.theme().colors().drop_target_background)
                            })
                            .drag_over::<DraggedSelection>(|bar, _, _, cx| {
                                bar.bg(cx.theme().colors().drop_target_background)
                            })
                            .on_drop(cx.listener(
                                move |this, dragged_tab: &DraggedTab, window, cx| {
                                    this.drag_split_direction = None;
                                    this.handle_tab_drop(dragged_tab, this.items.len(), window, cx)
                                },
                            ))
                            .on_drop(cx.listener(
                                move |this, selection: &DraggedSelection, window, cx| {
                                    this.drag_split_direction = None;
                                    this.handle_project_entry_drop(
                                        &selection.active_selection.entry_id,
                                        Some(tab_count),
                                        window,
                                        cx,
                                    )
                                },
                            ))
                            .on_drop(cx.listener(move |this, paths, window, cx| {
                                this.drag_split_direction = None;
                                this.handle_external_paths_drop(paths, window, cx)
                            }))
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                if event.up.click_count == 2 {
                                    window.dispatch_action(
                                        this.double_click_dispatch_action.boxed_clone(),
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
            .into_any_element()
    }

    pub fn render_menu_overlay(menu: &Entity<ContextMenu>) -> Div {
        div().absolute().bottom_0().right_0().size_0().child(
            deferred(anchored().anchor(Corner::TopRight).child(menu.clone())).with_priority(1),
        )
    }

    pub fn set_zoomed(&mut self, zoomed: bool, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        cx.notify();
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    fn handle_drag_move<T: 'static>(
        &mut self,
        event: &DragMoveEvent<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let can_split_predicate = self.can_split_predicate.take();
        let can_split = match &can_split_predicate {
            Some(can_split_predicate) => {
                can_split_predicate(self, event.dragged_item(), window, cx)
            }
            None => false,
        };
        self.can_split_predicate = can_split_predicate;
        if !can_split {
            return;
        }

        let rect = event.bounds.size;

        let size = event.bounds.size.width.min(event.bounds.size.height)
            * WorkspaceSettings::get_global(cx).drop_target_size;

        let relative_cursor = Point::new(
            event.event.position.x - event.bounds.left(),
            event.event.position.y - event.bounds.top(),
        );

        let direction = if relative_cursor.x < size
            || relative_cursor.x > rect.width - size
            || relative_cursor.y < size
            || relative_cursor.y > rect.height - size
        {
            [
                SplitDirection::Up,
                SplitDirection::Right,
                SplitDirection::Down,
                SplitDirection::Left,
            ]
            .iter()
            .min_by_key(|side| match side {
                SplitDirection::Up => relative_cursor.y,
                SplitDirection::Right => rect.width - relative_cursor.x,
                SplitDirection::Down => rect.height - relative_cursor.y,
                SplitDirection::Left => relative_cursor.x,
            })
            .cloned()
        } else {
            None
        };

        if direction != self.drag_split_direction {
            self.drag_split_direction = direction;
        }
    }

    pub fn handle_tab_drop(
        &mut self,
        dragged_tab: &DraggedTab,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(custom_drop_handle) = self.custom_drop_handle.clone() {
            if let ControlFlow::Break(()) = custom_drop_handle(self, dragged_tab, window, cx) {
                return;
            }
        }
        let mut to_pane = cx.entity().clone();
        let split_direction = self.drag_split_direction;
        let item_id = dragged_tab.item.item_id();
        if let Some(preview_item_id) = self.preview_item_id {
            if item_id == preview_item_id {
                self.set_preview_item_id(None, cx);
            }
        }

        let is_clone = cfg!(target_os = "macos") && window.modifiers().alt
            || cfg!(not(target_os = "macos")) && window.modifiers().control;

        let from_pane = dragged_tab.pane.clone();
        let from_ix = dragged_tab.ix;
        self.workspace
            .update(cx, |_, cx| {
                cx.defer_in(window, move |workspace, window, cx| {
                    if let Some(split_direction) = split_direction {
                        to_pane = workspace.split_pane(to_pane, split_direction, window, cx);
                    }
                    let database_id = workspace.database_id();
                    let was_pinned_in_from_pane = from_pane.read_with(cx, |pane, _| {
                        pane.index_for_item_id(item_id)
                            .is_some_and(|ix| pane.is_tab_pinned(ix))
                    });
                    let to_pane_old_length = to_pane.read(cx).items.len();
                    if is_clone {
                        let Some(item) = from_pane
                            .read(cx)
                            .items()
                            .find(|item| item.item_id() == item_id)
                            .map(|item| item.clone())
                        else {
                            return;
                        };
                        if let Some(item) = item.clone_on_split(database_id, window, cx) {
                            to_pane.update(cx, |pane, cx| {
                                pane.add_item(item, true, true, None, window, cx);
                            })
                        }
                    } else {
                        move_item(&from_pane, &to_pane, item_id, ix, true, window, cx);
                    }
                    to_pane.update(cx, |this, _| {
                        if to_pane == from_pane {
                            let moved_right = ix > from_ix;
                            let ix = if moved_right { ix - 1 } else { ix };
                            let is_pinned_in_to_pane = this.is_tab_pinned(ix);

                            if !was_pinned_in_from_pane && is_pinned_in_to_pane {
                                this.pinned_tab_count += 1;
                            } else if was_pinned_in_from_pane && !is_pinned_in_to_pane {
                                this.pinned_tab_count -= 1;
                            }
                        } else if this.items.len() >= to_pane_old_length {
                            let is_pinned_in_to_pane = this.is_tab_pinned(ix);
                            let item_created_pane = to_pane_old_length == 0;
                            let is_first_position = ix == 0;
                            let was_dropped_at_beginning = item_created_pane || is_first_position;
                            let should_remain_pinned = is_pinned_in_to_pane
                                || (was_pinned_in_from_pane && was_dropped_at_beginning);

                            if should_remain_pinned {
                                this.pinned_tab_count += 1;
                            }
                        }
                    });
                });
            })
            .log_err();
    }

    fn handle_dragged_selection_drop(
        &mut self,
        dragged_selection: &DraggedSelection,
        dragged_onto: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(custom_drop_handle) = self.custom_drop_handle.clone() {
            if let ControlFlow::Break(()) = custom_drop_handle(self, dragged_selection, window, cx)
            {
                return;
            }
        }
        self.handle_project_entry_drop(
            &dragged_selection.active_selection.entry_id,
            dragged_onto,
            window,
            cx,
        );
    }

    fn handle_project_entry_drop(
        &mut self,
        project_entry_id: &ProjectEntryId,
        target: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(custom_drop_handle) = self.custom_drop_handle.clone() {
            if let ControlFlow::Break(()) = custom_drop_handle(self, project_entry_id, window, cx) {
                return;
            }
        }
        let mut to_pane = cx.entity().clone();
        let split_direction = self.drag_split_direction;
        let project_entry_id = *project_entry_id;
        self.workspace
            .update(cx, |_, cx| {
                cx.defer_in(window, move |workspace, window, cx| {
                    if let Some(project_path) = workspace
                        .project()
                        .read(cx)
                        .path_for_entry(project_entry_id, cx)
                    {
                        let load_path_task = workspace.load_path(project_path.clone(), window, cx);
                        cx.spawn_in(window, async move |workspace, cx| {
                            if let Some((project_entry_id, build_item)) =
                                load_path_task.await.notify_async_err(cx)
                            {
                                let (to_pane, new_item_handle) = workspace
                                    .update_in(cx, |workspace, window, cx| {
                                        if let Some(split_direction) = split_direction {
                                            to_pane = workspace.split_pane(
                                                to_pane,
                                                split_direction,
                                                window,
                                                cx,
                                            );
                                        }
                                        let new_item_handle = to_pane.update(cx, |pane, cx| {
                                            pane.open_item(
                                                project_entry_id,
                                                project_path,
                                                true,
                                                false,
                                                true,
                                                target,
                                                window,
                                                cx,
                                                build_item,
                                            )
                                        });
                                        (to_pane, new_item_handle)
                                    })
                                    .log_err()?;
                                to_pane
                                    .update_in(cx, |this, window, cx| {
                                        let Some(index) = this.index_for_item(&*new_item_handle)
                                        else {
                                            return;
                                        };

                                        if target.map_or(false, |target| this.is_tab_pinned(target))
                                        {
                                            this.pin_tab_at(index, window, cx);
                                        }
                                    })
                                    .ok()?
                            }
                            Some(())
                        })
                        .detach();
                    };
                });
            })
            .log_err();
    }

    fn handle_external_paths_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(custom_drop_handle) = self.custom_drop_handle.clone() {
            if let ControlFlow::Break(()) = custom_drop_handle(self, paths, window, cx) {
                return;
            }
        }
        let mut to_pane = cx.entity().clone();
        let mut split_direction = self.drag_split_direction;
        let paths = paths.paths().to_vec();
        let is_remote = self
            .workspace
            .update(cx, |workspace, cx| {
                if workspace.project().read(cx).is_via_collab() {
                    workspace.show_error(
                        &anyhow::anyhow!("Cannot drop files on a remote project"),
                        cx,
                    );
                    true
                } else {
                    false
                }
            })
            .unwrap_or(true);
        if is_remote {
            return;
        }

        self.workspace
            .update(cx, |workspace, cx| {
                let fs = Arc::clone(workspace.project().read(cx).fs());
                cx.spawn_in(window, async move |workspace, cx| {
                    let mut is_file_checks = FuturesUnordered::new();
                    for path in &paths {
                        is_file_checks.push(fs.is_file(path))
                    }
                    let mut has_files_to_open = false;
                    while let Some(is_file) = is_file_checks.next().await {
                        if is_file {
                            has_files_to_open = true;
                            break;
                        }
                    }
                    drop(is_file_checks);
                    if !has_files_to_open {
                        split_direction = None;
                    }

                    if let Ok(open_task) = workspace.update_in(cx, |workspace, window, cx| {
                        if let Some(split_direction) = split_direction {
                            to_pane = workspace.split_pane(to_pane, split_direction, window, cx);
                        }
                        workspace.open_paths(
                            paths,
                            OpenOptions {
                                visible: Some(OpenVisible::OnlyDirectories),
                                ..Default::default()
                            },
                            Some(to_pane.downgrade()),
                            window,
                            cx,
                        )
                    }) {
                        let opened_items: Vec<_> = open_task.await;
                        _ = workspace.update(cx, |workspace, cx| {
                            for item in opened_items.into_iter().flatten() {
                                if let Err(e) = item {
                                    workspace.show_error(&e, cx);
                                }
                            }
                        });
                    }
                })
                .detach();
            })
            .log_err();
    }

    pub fn display_nav_history_buttons(&mut self, display: Option<bool>) {
        self.display_nav_history_buttons = display;
    }

    fn pinned_item_ids(&self) -> Vec<EntityId> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if self.is_tab_pinned(index) {
                    return Some(item.item_id());
                }

                None
            })
            .collect()
    }

    fn clean_item_ids(&self, cx: &mut Context<Pane>) -> Vec<EntityId> {
        self.items()
            .filter_map(|item| {
                if !item.is_dirty(cx) {
                    return Some(item.item_id());
                }

                None
            })
            .collect()
    }

    fn to_the_side_item_ids(&self, item_id: EntityId, side: Side) -> Vec<EntityId> {
        match side {
            Side::Left => self
                .items()
                .take_while(|item| item.item_id() != item_id)
                .map(|item| item.item_id())
                .collect(),
            Side::Right => self
                .items()
                .rev()
                .take_while(|item| item.item_id() != item_id)
                .map(|item| item.item_id())
                .collect(),
        }
    }

    pub fn drag_split_direction(&self) -> Option<SplitDirection> {
        self.drag_split_direction
    }

    pub fn set_zoom_out_on_close(&mut self, zoom_out_on_close: bool) {
        self.zoom_out_on_close = zoom_out_on_close;
    }
}

fn default_render_tab_bar_buttons(
    pane: &mut Pane,
    window: &mut Window,
    cx: &mut Context<Pane>,
) -> (Option<AnyElement>, Option<AnyElement>) {
    if !pane.has_focus(window, cx) && !pane.context_menu_focused(window, cx) {
        return (None, None);
    }
    // Ideally we would return a vec of elements here to pass directly to the [TabBar]'s
    // `end_slot`, but due to needing a view here that isn't possible.
    let right_children = h_flex()
        // Instead we need to replicate the spacing from the [TabBar]'s `end_slot` here.
        .gap(DynamicSpacing::Base04.rems(cx))
        .child(
            PopoverMenu::new("pane-tab-bar-popover-menu")
                .trigger_with_tooltip(
                    IconButton::new("plus", IconName::Plus).icon_size(IconSize::Small),
                    Tooltip::text("New..."),
                )
                .anchor(Corner::TopRight)
                .with_handle(pane.new_item_context_menu_handle.clone())
                .menu(move |window, cx| {
                    Some(ContextMenu::build(window, cx, |menu, _, _| {
                        menu.action("New File", NewFile.boxed_clone())
                            .action("Open File", ToggleFileFinder::default().boxed_clone())
                            .separator()
                            .action(
                                "Search Project",
                                DeploySearch {
                                    replace_enabled: false,
                                    included_files: None,
                                    excluded_files: None,
                                }
                                .boxed_clone(),
                            )
                            .action("Search Symbols", ToggleProjectSymbols.boxed_clone())
                            .separator()
                            .action("New Terminal", NewTerminal.boxed_clone())
                    }))
                }),
        )
        .child(
            PopoverMenu::new("pane-tab-bar-split")
                .trigger_with_tooltip(
                    IconButton::new("split", IconName::Split).icon_size(IconSize::Small),
                    Tooltip::text("Split Pane"),
                )
                .anchor(Corner::TopRight)
                .with_handle(pane.split_item_context_menu_handle.clone())
                .menu(move |window, cx| {
                    ContextMenu::build(window, cx, |menu, _, _| {
                        menu.action("Split Right", SplitRight.boxed_clone())
                            .action("Split Left", SplitLeft.boxed_clone())
                            .action("Split Up", SplitUp.boxed_clone())
                            .action("Split Down", SplitDown.boxed_clone())
                    })
                    .into()
                }),
        )
        .child({
            let zoomed = pane.is_zoomed();
            IconButton::new("toggle_zoom", IconName::Maximize)
                .icon_size(IconSize::Small)
                .toggle_state(zoomed)
                .selected_icon(IconName::Minimize)
                .on_click(cx.listener(|pane, _, window, cx| {
                    pane.toggle_zoom(&crate::ToggleZoom, window, cx);
                }))
                .tooltip(move |window, cx| {
                    Tooltip::for_action(
                        if zoomed { "Zoom Out" } else { "Zoom In" },
                        &ToggleZoom,
                        window,
                        cx,
                    )
                })
        })
        .into_any_element()
        .into();
    (None, right_children)
}

impl Focusable for Pane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("Pane");
        if self.active_item().is_none() {
            key_context.add("EmptyPane");
        }

        let should_display_tab_bar = self.should_display_tab_bar.clone();
        let display_tab_bar = should_display_tab_bar(window, cx);
        let Some(project) = self.project.upgrade() else {
            return div().track_focus(&self.focus_handle(cx));
        };
        let is_local = project.read(cx).is_local();

        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex_none()
            .overflow_hidden()
            .on_action(cx.listener(|pane, _: &AlternateFile, window, cx| {
                pane.alternate_file(window, cx);
            }))
            .on_action(
                cx.listener(|pane, _: &SplitLeft, _, cx| pane.split(SplitDirection::Left, cx)),
            )
            .on_action(cx.listener(|pane, _: &SplitUp, _, cx| pane.split(SplitDirection::Up, cx)))
            .on_action(cx.listener(|pane, _: &SplitHorizontal, _, cx| {
                pane.split(SplitDirection::horizontal(cx), cx)
            }))
            .on_action(cx.listener(|pane, _: &SplitVertical, _, cx| {
                pane.split(SplitDirection::vertical(cx), cx)
            }))
            .on_action(
                cx.listener(|pane, _: &SplitRight, _, cx| pane.split(SplitDirection::Right, cx)),
            )
            .on_action(
                cx.listener(|pane, _: &SplitDown, _, cx| pane.split(SplitDirection::Down, cx)),
            )
            .on_action(
                cx.listener(|pane, _: &GoBack, window, cx| pane.navigate_backward(window, cx)),
            )
            .on_action(
                cx.listener(|pane, _: &GoForward, window, cx| pane.navigate_forward(window, cx)),
            )
            .on_action(cx.listener(|_, _: &JoinIntoNext, _, cx| {
                cx.emit(Event::JoinIntoNext);
            }))
            .on_action(cx.listener(|_, _: &JoinAll, _, cx| {
                cx.emit(Event::JoinAll);
            }))
            .on_action(cx.listener(Pane::toggle_zoom))
            .on_action(
                cx.listener(|pane: &mut Pane, action: &ActivateItem, window, cx| {
                    pane.activate_item(
                        action.0.min(pane.items.len().saturating_sub(1)),
                        true,
                        true,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(
                cx.listener(|pane: &mut Pane, _: &ActivateLastItem, window, cx| {
                    pane.activate_item(pane.items.len().saturating_sub(1), true, true, window, cx);
                }),
            )
            .on_action(
                cx.listener(|pane: &mut Pane, _: &ActivatePreviousItem, window, cx| {
                    pane.activate_prev_item(true, window, cx);
                }),
            )
            .on_action(
                cx.listener(|pane: &mut Pane, _: &ActivateNextItem, window, cx| {
                    pane.activate_next_item(true, window, cx);
                }),
            )
            .on_action(
                cx.listener(|pane, _: &SwapItemLeft, window, cx| pane.swap_item_left(window, cx)),
            )
            .on_action(
                cx.listener(|pane, _: &SwapItemRight, window, cx| pane.swap_item_right(window, cx)),
            )
            .on_action(cx.listener(|pane, action, window, cx| {
                pane.toggle_pin_tab(action, window, cx);
            }))
            .on_action(cx.listener(|pane, action, window, cx| {
                pane.unpin_all_tabs(action, window, cx);
            }))
            .when(PreviewTabsSettings::get_global(cx).enabled, |this| {
                this.on_action(cx.listener(|pane: &mut Pane, _: &TogglePreviewTab, _, cx| {
                    if let Some(active_item_id) = pane.active_item().map(|i| i.item_id()) {
                        if pane.is_active_preview_item(active_item_id) {
                            pane.set_preview_item_id(None, cx);
                        } else {
                            pane.set_preview_item_id(Some(active_item_id), cx);
                        }
                    }
                }))
            })
            .on_action(
                cx.listener(|pane: &mut Self, action: &CloseActiveItem, window, cx| {
                    pane.close_active_item(action, window, cx)
                        .detach_and_log_err(cx)
                }),
            )
            .on_action(
                cx.listener(|pane: &mut Self, action: &CloseInactiveItems, window, cx| {
                    pane.close_inactive_items(action, window, cx)
                        .detach_and_log_err(cx);
                }),
            )
            .on_action(
                cx.listener(|pane: &mut Self, action: &CloseCleanItems, window, cx| {
                    pane.close_clean_items(action, window, cx)
                        .detach_and_log_err(cx)
                }),
            )
            .on_action(cx.listener(
                |pane: &mut Self, action: &CloseItemsToTheLeft, window, cx| {
                    pane.close_items_to_the_left_by_id(None, action, window, cx)
                        .detach_and_log_err(cx)
                },
            ))
            .on_action(cx.listener(
                |pane: &mut Self, action: &CloseItemsToTheRight, window, cx| {
                    pane.close_items_to_the_right_by_id(None, action, window, cx)
                        .detach_and_log_err(cx)
                },
            ))
            .on_action(
                cx.listener(|pane: &mut Self, action: &CloseAllItems, window, cx| {
                    pane.close_all_items(action, window, cx)
                        .detach_and_log_err(cx)
                }),
            )
            .on_action(
                cx.listener(|pane: &mut Self, action: &RevealInProjectPanel, _, cx| {
                    let entry_id = action
                        .entry_id
                        .map(ProjectEntryId::from_proto)
                        .or_else(|| pane.active_item()?.project_entry_ids(cx).first().copied());
                    if let Some(entry_id) = entry_id {
                        pane.project
                            .update(cx, |_, cx| {
                                cx.emit(project::Event::RevealInProjectPanel(entry_id))
                            })
                            .ok();
                    }
                }),
            )
            .on_action(cx.listener(|_, _: &menu::Cancel, window, cx| {
                if cx.stop_active_drag(window) {
                    return;
                } else {
                    cx.propagate();
                }
            }))
            .when(self.active_item().is_some() && display_tab_bar, |pane| {
                pane.child((self.render_tab_bar.clone())(self, window, cx))
            })
            .child({
                let has_worktrees = project.read(cx).visible_worktrees(cx).next().is_some();
                // main content
                div()
                    .flex_1()
                    .relative()
                    .group("")
                    .overflow_hidden()
                    .on_drag_move::<DraggedTab>(cx.listener(Self::handle_drag_move))
                    .on_drag_move::<DraggedSelection>(cx.listener(Self::handle_drag_move))
                    .when(is_local, |div| {
                        div.on_drag_move::<ExternalPaths>(cx.listener(Self::handle_drag_move))
                    })
                    .map(|div| {
                        if let Some(item) = self.active_item() {
                            div.id("pane_placeholder")
                                .v_flex()
                                .size_full()
                                .overflow_hidden()
                                // .child(self.toolbar.clone()) // Hidden: toolbar moved to title bar
                                .child(item.to_any())
                        } else {
                            let placeholder = div
                                .id("pane_placeholder")
                                .h_flex()
                                .size_full()
                                .justify_center()
                                .on_click(cx.listener(
                                    move |this, event: &ClickEvent, window, cx| {
                                        if event.up.click_count == 2 {
                                            window.dispatch_action(
                                                this.double_click_dispatch_action.boxed_clone(),
                                                cx,
                                            );
                                        }
                                    },
                                ));
                            if has_worktrees {
                                placeholder
                            } else {
                                placeholder.child(
                                    Label::new("Open a file or project to get started.")
                                        .color(Color::Muted),
                                )
                            }
                        }
                    })
                    .child(
                        // drag target
                        div()
                            .invisible()
                            .absolute()
                            .bg(cx.theme().colors().drop_target_background)
                            .group_drag_over::<DraggedTab>("", |style| style.visible())
                            .group_drag_over::<DraggedSelection>("", |style| style.visible())
                            .when(is_local, |div| {
                                div.group_drag_over::<ExternalPaths>("", |style| style.visible())
                            })
                            .when_some(self.can_drop_predicate.clone(), |this, p| {
                                this.can_drop(move |a, window, cx| p(a, window, cx))
                            })
                            .on_drop(cx.listener(move |this, dragged_tab, window, cx| {
                                this.handle_tab_drop(
                                    dragged_tab,
                                    this.active_item_index(),
                                    window,
                                    cx,
                                )
                            }))
                            .on_drop(cx.listener(
                                move |this, selection: &DraggedSelection, window, cx| {
                                    this.handle_dragged_selection_drop(selection, None, window, cx)
                                },
                            ))
                            .on_drop(cx.listener(move |this, paths, window, cx| {
                                this.handle_external_paths_drop(paths, window, cx)
                            }))
                            .map(|div| {
                                let size = DefiniteLength::Fraction(0.5);
                                match self.drag_split_direction {
                                    None => div.top_0().right_0().bottom_0().left_0(),
                                    Some(SplitDirection::Up) => {
                                        div.top_0().left_0().right_0().h(size)
                                    }
                                    Some(SplitDirection::Down) => {
                                        div.left_0().bottom_0().right_0().h(size)
                                    }
                                    Some(SplitDirection::Left) => {
                                        div.top_0().left_0().bottom_0().w(size)
                                    }
                                    Some(SplitDirection::Right) => {
                                        div.top_0().bottom_0().right_0().w(size)
                                    }
                                }
                            }),
                    )
            })
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|pane, _, window, cx| {
                    if let Some(workspace) = pane.workspace.upgrade() {
                        let pane = cx.entity().downgrade();
                        window.defer(cx, move |window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace.go_back(pane, window, cx).detach_and_log_err(cx)
                            })
                        })
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|pane, _, window, cx| {
                    if let Some(workspace) = pane.workspace.upgrade() {
                        let pane = cx.entity().downgrade();
                        window.defer(cx, move |window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace
                                    .go_forward(pane, window, cx)
                                    .detach_and_log_err(cx)
                            })
                        })
                    }
                }),
            )
    }
}

impl ItemNavHistory {
    pub fn push<D: 'static + Send + Any>(&mut self, data: Option<D>, cx: &mut App) {
        if self
            .item
            .upgrade()
            .is_some_and(|item| item.include_in_nav_history())
        {
            self.history
                .push(data, self.item.clone(), self.is_preview, cx);
        }
    }

    pub fn pop_backward(&mut self, cx: &mut App) -> Option<NavigationEntry> {
        self.history.pop(NavigationMode::GoingBack, cx)
    }

    pub fn pop_forward(&mut self, cx: &mut App) -> Option<NavigationEntry> {
        self.history.pop(NavigationMode::GoingForward, cx)
    }
}

impl NavHistory {
    pub fn for_each_entry(
        &self,
        cx: &App,
        mut f: impl FnMut(&NavigationEntry, (ProjectPath, Option<PathBuf>)),
    ) {
        let borrowed_history = self.0.lock();
        borrowed_history
            .forward_stack
            .iter()
            .chain(borrowed_history.backward_stack.iter())
            .chain(borrowed_history.closed_stack.iter())
            .for_each(|entry| {
                if let Some(project_and_abs_path) =
                    borrowed_history.paths_by_item.get(&entry.item.id())
                {
                    f(entry, project_and_abs_path.clone());
                } else if let Some(item) = entry.item.upgrade() {
                    if let Some(path) = item.project_path(cx) {
                        f(entry, (path, None));
                    }
                }
            })
    }

    pub fn set_mode(&mut self, mode: NavigationMode) {
        self.0.lock().mode = mode;
    }

    pub fn mode(&self) -> NavigationMode {
        self.0.lock().mode
    }

    pub fn disable(&mut self) {
        self.0.lock().mode = NavigationMode::Disabled;
    }

    pub fn enable(&mut self) {
        self.0.lock().mode = NavigationMode::Normal;
    }

    pub fn pop(&mut self, mode: NavigationMode, cx: &mut App) -> Option<NavigationEntry> {
        let mut state = self.0.lock();
        let entry = match mode {
            NavigationMode::Normal | NavigationMode::Disabled | NavigationMode::ClosingItem => {
                return None;
            }
            NavigationMode::GoingBack => &mut state.backward_stack,
            NavigationMode::GoingForward => &mut state.forward_stack,
            NavigationMode::ReopeningClosedItem => &mut state.closed_stack,
        }
        .pop_back();
        if entry.is_some() {
            state.did_update(cx);
        }
        entry
    }

    pub fn push<D: 'static + Send + Any>(
        &mut self,
        data: Option<D>,
        item: Arc<dyn WeakItemHandle>,
        is_preview: bool,
        cx: &mut App,
    ) {
        let state = &mut *self.0.lock();
        match state.mode {
            NavigationMode::Disabled => {}
            NavigationMode::Normal | NavigationMode::ReopeningClosedItem => {
                if state.backward_stack.len() >= MAX_NAVIGATION_HISTORY_LEN {
                    state.backward_stack.pop_front();
                }
                state.backward_stack.push_back(NavigationEntry {
                    item,
                    data: data.map(|data| Box::new(data) as Box<dyn Any + Send>),
                    timestamp: state.next_timestamp.fetch_add(1, Ordering::SeqCst),
                    is_preview,
                });
                state.forward_stack.clear();
            }
            NavigationMode::GoingBack => {
                if state.forward_stack.len() >= MAX_NAVIGATION_HISTORY_LEN {
                    state.forward_stack.pop_front();
                }
                state.forward_stack.push_back(NavigationEntry {
                    item,
                    data: data.map(|data| Box::new(data) as Box<dyn Any + Send>),
                    timestamp: state.next_timestamp.fetch_add(1, Ordering::SeqCst),
                    is_preview,
                });
            }
            NavigationMode::GoingForward => {
                if state.backward_stack.len() >= MAX_NAVIGATION_HISTORY_LEN {
                    state.backward_stack.pop_front();
                }
                state.backward_stack.push_back(NavigationEntry {
                    item,
                    data: data.map(|data| Box::new(data) as Box<dyn Any + Send>),
                    timestamp: state.next_timestamp.fetch_add(1, Ordering::SeqCst),
                    is_preview,
                });
            }
            NavigationMode::ClosingItem => {
                if state.closed_stack.len() >= MAX_NAVIGATION_HISTORY_LEN {
                    state.closed_stack.pop_front();
                }
                state.closed_stack.push_back(NavigationEntry {
                    item,
                    data: data.map(|data| Box::new(data) as Box<dyn Any + Send>),
                    timestamp: state.next_timestamp.fetch_add(1, Ordering::SeqCst),
                    is_preview,
                });
            }
        }
        state.did_update(cx);
    }

    pub fn remove_item(&mut self, item_id: EntityId) {
        let mut state = self.0.lock();
        state.paths_by_item.remove(&item_id);
        state
            .backward_stack
            .retain(|entry| entry.item.id() != item_id);
        state
            .forward_stack
            .retain(|entry| entry.item.id() != item_id);
        state
            .closed_stack
            .retain(|entry| entry.item.id() != item_id);
    }

    pub fn path_for_item(&self, item_id: EntityId) -> Option<(ProjectPath, Option<PathBuf>)> {
        self.0.lock().paths_by_item.get(&item_id).cloned()
    }
}

impl NavHistoryState {
    pub fn did_update(&self, cx: &mut App) {
        if let Some(pane) = self.pane.upgrade() {
            cx.defer(move |cx| {
                pane.update(cx, |pane, cx| pane.history_updated(cx));
            });
        }
    }
}

fn dirty_message_for(buffer_path: Option<ProjectPath>) -> String {
    let path = buffer_path
        .as_ref()
        .and_then(|p| {
            p.path
                .to_str()
                .and_then(|s| if s.is_empty() { None } else { Some(s) })
        })
        .unwrap_or("This buffer");
    let path = truncate_and_remove_front(path, 80);
    format!("{path} contains unsaved edits. Do you want to save it?")
}

pub fn tab_details(items: &[Box<dyn ItemHandle>], _window: &Window, cx: &App) -> Vec<usize> {
    let mut tab_details = items.iter().map(|_| 0).collect::<Vec<_>>();
    let mut tab_descriptions = HashMap::default();
    let mut done = false;
    while !done {
        done = true;

        // Store item indices by their tab description.
        for (ix, (item, detail)) in items.iter().zip(&tab_details).enumerate() {
            let description = item.tab_content_text(*detail, cx);
            if *detail == 0 || description != item.tab_content_text(detail - 1, cx) {
                tab_descriptions
                    .entry(description)
                    .or_insert(Vec::new())
                    .push(ix);
            }
        }

        // If two or more items have the same tab description, increase their level
        // of detail and try again.
        for (_, item_ixs) in tab_descriptions.drain() {
            if item_ixs.len() > 1 {
                done = false;
                for ix in item_ixs {
                    tab_details[ix] += 1;
                }
            }
        }
    }

    tab_details
}

pub fn render_item_indicator(item: Box<dyn ItemHandle>, cx: &App) -> Option<Indicator> {
    maybe!({
        let indicator_color = match (item.has_conflict(cx), item.is_dirty(cx)) {
            (true, _) => Color::Warning,
            (_, true) => Color::Accent,
            (false, false) => return None,
        };

        Some(Indicator::dot().color(indicator_color))
    })
}

impl Render for DraggedTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = ThemeSettings::get_global(cx).ui_font.clone();
        let label = self.item.tab_content(
            TabContentParams {
                detail: Some(self.detail),
                selected: false,
                preview: false,
                deemphasized: false,
            },
            window,
            cx,
        );
        Tab::new("")
            .toggle_state(self.is_active)
            .child(label)
            .render(window, cx)
            .font(ui_font)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;

    use super::*;
    use crate::item::test::{TestItem, TestProjectItem};
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use settings::SettingsStore;
    use theme::LoadThemes;
    use util::TryFutureExt;

    #[gpui::test]
    async fn test_add_item_capped_to_max_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        for i in 0..7 {
            add_labeled_item(&pane, format!("{}", i).as_str(), false, cx);
        }

        set_max_tabs(cx, Some(5));
        add_labeled_item(&pane, "7", false, cx);
        // Remove items to respect the max tab cap.
        assert_item_labels(&pane, ["3", "4", "5", "6", "7*"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(0, false, false, window, cx);
        });
        add_labeled_item(&pane, "X", false, cx);
        // Respect activation order.
        assert_item_labels(&pane, ["3", "X*", "5", "6", "7"], cx);

        for i in 0..7 {
            add_labeled_item(&pane, format!("D{}", i).as_str(), true, cx);
        }
        // Keeps dirty items, even over max tab cap.
        assert_item_labels(
            &pane,
            ["D0^", "D1^", "D2^", "D3^", "D4^", "D5^", "D6*^"],
            cx,
        );

        set_max_tabs(cx, None);
        for i in 0..7 {
            add_labeled_item(&pane, format!("N{}", i).as_str(), false, cx);
        }
        // No cap when max tabs is None.
        assert_item_labels(
            &pane,
            [
                "D0^", "D1^", "D2^", "D3^", "D4^", "D5^", "D6^", "N0", "N1", "N2", "N3", "N4",
                "N5", "N6*",
            ],
            cx,
        );
    }

    #[gpui::test]
    async fn test_reduce_max_tabs_closes_existing_items(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        let item_c = add_labeled_item(&pane, "C", false, cx);
        let item_d = add_labeled_item(&pane, "D", false, cx);
        add_labeled_item(&pane, "E", false, cx);
        add_labeled_item(&pane, "Settings", false, cx);
        assert_item_labels(&pane, ["A", "B", "C", "D", "E", "Settings*"], cx);

        set_max_tabs(cx, Some(5));
        assert_item_labels(&pane, ["B", "C", "D", "E", "Settings*"], cx);

        set_max_tabs(cx, Some(4));
        assert_item_labels(&pane, ["C", "D", "E", "Settings*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_d.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C!", "D!", "E", "Settings*"], cx);

        set_max_tabs(cx, Some(2));
        assert_item_labels(&pane, ["C!", "D!", "Settings*"], cx);
    }

    #[gpui::test]
    async fn test_allow_pinning_dirty_item_at_max_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(1));
        let item_a = add_labeled_item(&pane, "A", true, cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A*^!"], cx);
    }

    #[gpui::test]
    async fn test_allow_pinning_non_dirty_item_at_max_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(1));
        let item_a = add_labeled_item(&pane, "A", false, cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A*!"], cx);
    }

    #[gpui::test]
    async fn test_pin_tabs_incrementally_at_max_capacity(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(3));

        let item_a = add_labeled_item(&pane, "A", false, cx);
        assert_item_labels(&pane, ["A*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A*!"], cx);

        let item_b = add_labeled_item(&pane, "B", false, cx);
        assert_item_labels(&pane, ["A!", "B*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B*!"], cx);

        let item_c = add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B!", "C*!"], cx);
    }

    #[gpui::test]
    async fn test_pin_tabs_left_to_right_after_opening_at_max_capacity(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(3));

        let item_a = add_labeled_item(&pane, "A", false, cx);
        assert_item_labels(&pane, ["A*"], cx);

        let item_b = add_labeled_item(&pane, "B", false, cx);
        assert_item_labels(&pane, ["A", "B*"], cx);

        let item_c = add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B!", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B!", "C*!"], cx);
    }

    #[gpui::test]
    async fn test_pin_tabs_right_to_left_after_opening_at_max_capacity(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(3));

        let item_a = add_labeled_item(&pane, "A", false, cx);
        assert_item_labels(&pane, ["A*"], cx);

        let item_b = add_labeled_item(&pane, "B", false, cx);
        assert_item_labels(&pane, ["A", "B*"], cx);

        let item_c = add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C*!", "A", "B"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C*!", "B!", "A"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C*!", "B!", "A!"], cx);
    }

    #[gpui::test]
    async fn test_pinned_tabs_never_closed_at_max_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        let item_a = add_labeled_item(&pane, "A", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });

        let item_b = add_labeled_item(&pane, "B", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });

        add_labeled_item(&pane, "C", false, cx);
        add_labeled_item(&pane, "D", false, cx);
        add_labeled_item(&pane, "E", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C", "D", "E*"], cx);

        set_max_tabs(cx, Some(3));
        add_labeled_item(&pane, "F", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "F*"], cx);

        add_labeled_item(&pane, "G", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "G*"], cx);

        add_labeled_item(&pane, "H", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "H*"], cx);
    }

    #[gpui::test]
    async fn test_always_allows_one_unpinned_item_over_max_tabs_regardless_of_pinned_count(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(3));

        let item_a = add_labeled_item(&pane, "A", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });

        let item_b = add_labeled_item(&pane, "B", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });

        let item_c = add_labeled_item(&pane, "C", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });

        assert_item_labels(&pane, ["A!", "B!", "C*!"], cx);

        let item_d = add_labeled_item(&pane, "D", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C!", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_d.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B!", "C!", "D*!"], cx);

        add_labeled_item(&pane, "E", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C!", "D!", "E*"], cx);

        add_labeled_item(&pane, "F", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C!", "D!", "F*"], cx);
    }

    #[gpui::test]
    async fn test_can_open_one_item_when_all_tabs_are_dirty_at_max(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_max_tabs(cx, Some(3));

        add_labeled_item(&pane, "A", true, cx);
        assert_item_labels(&pane, ["A*^"], cx);

        add_labeled_item(&pane, "B", true, cx);
        assert_item_labels(&pane, ["A^", "B*^"], cx);

        add_labeled_item(&pane, "C", true, cx);
        assert_item_labels(&pane, ["A^", "B^", "C*^"], cx);

        add_labeled_item(&pane, "D", false, cx);
        assert_item_labels(&pane, ["A^", "B^", "C^", "D*"], cx);

        add_labeled_item(&pane, "E", false, cx);
        assert_item_labels(&pane, ["A^", "B^", "C^", "E*"], cx);

        add_labeled_item(&pane, "F", false, cx);
        assert_item_labels(&pane, ["A^", "B^", "C^", "F*"], cx);

        add_labeled_item(&pane, "G", true, cx);
        assert_item_labels(&pane, ["A^", "B^", "C^", "G*^"], cx);
    }

    #[gpui::test]
    async fn test_toggle_pin_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_labeled_items(&pane, ["A", "B*", "C"], cx);
        assert_item_labels(&pane, ["A", "B*", "C"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.toggle_pin_tab(&TogglePinTab, window, cx);
        });
        assert_item_labels(&pane, ["B*!", "A", "C"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.toggle_pin_tab(&TogglePinTab, window, cx);
        });
        assert_item_labels(&pane, ["B*", "A", "C"], cx);
    }

    #[gpui::test]
    async fn test_unpin_all_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Unpin all, in an empty pane
        pane.update_in(cx, |pane, window, cx| {
            pane.unpin_all_tabs(&UnpinAllTabs, window, cx);
        });

        assert_item_labels(&pane, [], cx);

        let item_a = add_labeled_item(&pane, "A", false, cx);
        let item_b = add_labeled_item(&pane, "B", false, cx);
        let item_c = add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        // Unpin all, when no tabs are pinned
        pane.update_in(cx, |pane, window, cx| {
            pane.unpin_all_tabs(&UnpinAllTabs, window, cx);
        });

        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        // Pin inactive tabs only
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B!", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.unpin_all_tabs(&UnpinAllTabs, window, cx);
        });

        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        // Pin all tabs
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B!", "C*!"], cx);

        // Activate middle tab
        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(1, false, false, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B*!", "C!"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.unpin_all_tabs(&UnpinAllTabs, window, cx);
        });

        // Order has not changed
        assert_item_labels(&pane, ["A", "B*", "C"], cx);
    }

    #[gpui::test]
    async fn test_pinning_active_tab_without_position_change_maintains_focus(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A
        let item_a = add_labeled_item(&pane, "A", false, cx);
        assert_item_labels(&pane, ["A*"], cx);

        // Add B
        add_labeled_item(&pane, "B", false, cx);
        assert_item_labels(&pane, ["A", "B*"], cx);

        // Activate A again
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.activate_item(ix, true, true, window, cx);
        });
        assert_item_labels(&pane, ["A*", "B"], cx);

        // Pin A - remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A*!", "B"], cx);

        // Unpin A - remain active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.unpin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A*", "B"], cx);
    }

    #[gpui::test]
    async fn test_pinning_active_tab_with_position_change_maintains_focus(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B, C
        add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        let item_c = add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        // Pin C - moves to pinned area, remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C*!", "A", "B"], cx);

        // Unpin C - moves after pinned area, remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.unpin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C*", "A", "B"], cx);
    }

    #[gpui::test]
    async fn test_pinning_inactive_tab_without_position_change_preserves_existing_focus(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B
        let item_a = add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        assert_item_labels(&pane, ["A", "B*"], cx);

        // Pin A - already in pinned area, B remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B*"], cx);

        // Unpin A - stays in place, B remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.unpin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A", "B*"], cx);
    }

    #[gpui::test]
    async fn test_pinning_inactive_tab_with_position_change_preserves_existing_focus(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B, C
        add_labeled_item(&pane, "A", false, cx);
        let item_b = add_labeled_item(&pane, "B", false, cx);
        let item_c = add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        // Activate B
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.activate_item(ix, true, true, window, cx);
        });
        assert_item_labels(&pane, ["A", "B*", "C"], cx);

        // Pin C - moves to pinned area, B remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C!", "A", "B*"], cx);

        // Unpin C - moves after pinned area, B remains active
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.unpin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["C", "A", "B*"], cx);
    }

    #[gpui::test]
    async fn test_drag_unpinned_tab_to_split_creates_pane_with_unpinned_tab(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B. Pin B. Activate A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        let item_b = add_labeled_item(&pane_a, "B", false, cx);

        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.activate_item(ix, true, true, window, cx);
        });

        // Drag A to create new split
        pane_a.update_in(cx, |pane, window, cx| {
            pane.drag_split_direction = Some(SplitDirection::Right);

            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A should be moved to new pane. B should remain pinned, A should not be pinned
        let (pane_a, pane_b) = workspace.read_with(cx, |workspace, _| {
            let panes = workspace.panes();
            (panes[0].clone(), panes[1].clone())
        });
        assert_item_labels(&pane_a, ["B*!"], cx);
        assert_item_labels(&pane_b, ["A*"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_to_split_creates_pane_with_pinned_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B. Pin both. Activate A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        let item_b = add_labeled_item(&pane_a, "B", false, cx);

        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.activate_item(ix, true, true, window, cx);
        });
        assert_item_labels(&pane_a, ["A*!", "B!"], cx);

        // Drag A to create new split
        pane_a.update_in(cx, |pane, window, cx| {
            pane.drag_split_direction = Some(SplitDirection::Right);

            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A should be moved to new pane. Both A and B should still be pinned
        let (pane_a, pane_b) = workspace.read_with(cx, |workspace, _| {
            let panes = workspace.panes();
            (panes[0].clone(), panes[1].clone())
        });
        assert_item_labels(&pane_a, ["B*!"], cx);
        assert_item_labels(&pane_b, ["A*!"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_into_existing_panes_pinned_region(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A to pane A and pin
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A*!"], cx);

        // Add B to pane B and pin
        let pane_b = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane_a.clone(), SplitDirection::Right, window, cx)
        });
        let item_b = add_labeled_item(&pane_b, "B", false, cx);
        pane_b.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_b, ["B*!"], cx);

        // Move A from pane A to pane B's pinned region
        pane_b.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A should stay pinned
        assert_item_labels(&pane_a, [], cx);
        assert_item_labels(&pane_b, ["A*!", "B!"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_into_existing_panes_unpinned_region(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A to pane A and pin
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A*!"], cx);

        // Create pane B with pinned item B
        let pane_b = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane_a.clone(), SplitDirection::Right, window, cx)
        });
        let item_b = add_labeled_item(&pane_b, "B", false, cx);
        assert_item_labels(&pane_b, ["B*"], cx);

        pane_b.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_b, ["B*!"], cx);

        // Move A from pane A to pane B's unpinned region
        pane_b.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 1, window, cx);
        });

        // A should become pinned
        assert_item_labels(&pane_a, [], cx);
        assert_item_labels(&pane_b, ["B!", "A*"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_into_existing_panes_first_position_with_no_pinned_tabs(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A to pane A and pin
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A*!"], cx);

        // Add B to pane B
        let pane_b = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane_a.clone(), SplitDirection::Right, window, cx)
        });
        add_labeled_item(&pane_b, "B", false, cx);
        assert_item_labels(&pane_b, ["B*"], cx);

        // Move A from pane A to position 0 in pane B, indicating it should stay pinned
        pane_b.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A should stay pinned
        assert_item_labels(&pane_a, [], cx);
        assert_item_labels(&pane_b, ["A*!", "B"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_into_existing_pane_at_max_capacity_closes_unpinned_tabs(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        set_max_tabs(cx, Some(2));

        // Add A, B to pane A. Pin both
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        let item_b = add_labeled_item(&pane_a, "B", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A!", "B*!"], cx);

        // Add C, D to pane B. Pin both
        let pane_b = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane_a.clone(), SplitDirection::Right, window, cx)
        });
        let item_c = add_labeled_item(&pane_b, "C", false, cx);
        let item_d = add_labeled_item(&pane_b, "D", false, cx);
        pane_b.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_d.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_b, ["C!", "D*!"], cx);

        // Add a third unpinned item to pane B (exceeds max tabs), but is allowed,
        // as we allow 1 tab over max if the others are pinned or dirty
        add_labeled_item(&pane_b, "E", false, cx);
        assert_item_labels(&pane_b, ["C!", "D!", "E*"], cx);

        // Drag pinned A from pane A to position 0 in pane B
        pane_b.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // E (unpinned) should be closed, leaving 3 pinned items
        assert_item_labels(&pane_a, ["B*!"], cx);
        assert_item_labels(&pane_b, ["A*!", "C!", "D!"], cx);
    }

    #[gpui::test]
    async fn test_drag_last_pinned_tab_to_same_position_stays_pinned(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A to pane A and pin it
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A*!"], cx);

        // Drag pinned A to position 1 (directly to the right) in the same pane
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 1, window, cx);
        });

        // A should still be pinned and active
        assert_item_labels(&pane_a, ["A*!"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_beyond_last_pinned_tab_in_same_pane_stays_pinned(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B to pane A and pin both
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        let item_b = add_labeled_item(&pane_a, "B", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A!", "B*!"], cx);

        // Drag pinned A right of B in the same pane
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 2, window, cx);
        });

        // A stays pinned
        assert_item_labels(&pane_a, ["B!", "A*!"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_beyond_unpinned_tab_in_same_pane_becomes_unpinned(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B to pane A and pin A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        add_labeled_item(&pane_a, "B", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A!", "B*"], cx);

        // Drag pinned A right of B in the same pane
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 2, window, cx);
        });

        // A becomes unpinned
        assert_item_labels(&pane_a, ["B", "A*"], cx);
    }

    #[gpui::test]
    async fn test_drag_unpinned_tab_in_front_of_pinned_tab_in_same_pane_becomes_pinned(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B to pane A and pin A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        let item_b = add_labeled_item(&pane_a, "B", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A!", "B*"], cx);

        // Drag pinned B left of A in the same pane
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_b.boxed_clone(),
                ix: 1,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A becomes unpinned
        assert_item_labels(&pane_a, ["B*!", "A!"], cx);
    }

    #[gpui::test]
    async fn test_drag_unpinned_tab_to_the_pinned_region_stays_pinned(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B, C to pane A and pin A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        add_labeled_item(&pane_a, "B", false, cx);
        let item_c = add_labeled_item(&pane_a, "C", false, cx);
        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A!", "B", "C*"], cx);

        // Drag pinned C left of B in the same pane
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_c.boxed_clone(),
                ix: 2,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 1, window, cx);
        });

        // A stays pinned, B and C remain unpinned
        assert_item_labels(&pane_a, ["A!", "C*", "B"], cx);
    }

    #[gpui::test]
    async fn test_drag_unpinned_tab_into_existing_panes_pinned_region(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add unpinned item A to pane A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        assert_item_labels(&pane_a, ["A*"], cx);

        // Create pane B with pinned item B
        let pane_b = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane_a.clone(), SplitDirection::Right, window, cx)
        });
        let item_b = add_labeled_item(&pane_b, "B", false, cx);
        pane_b.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_b, ["B*!"], cx);

        // Move A from pane A to pane B's pinned region
        pane_b.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A should become pinned since it was dropped in the pinned region
        assert_item_labels(&pane_a, [], cx);
        assert_item_labels(&pane_b, ["A*!", "B!"], cx);
    }

    #[gpui::test]
    async fn test_drag_unpinned_tab_into_existing_panes_unpinned_region(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add unpinned item A to pane A
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        assert_item_labels(&pane_a, ["A*"], cx);

        // Create pane B with one pinned item B
        let pane_b = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane_a.clone(), SplitDirection::Right, window, cx)
        });
        let item_b = add_labeled_item(&pane_b, "B", false, cx);
        pane_b.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_b, ["B*!"], cx);

        // Move A from pane A to pane B's unpinned region
        pane_b.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 1, window, cx);
        });

        // A should remain unpinned since it was dropped outside the pinned region
        assert_item_labels(&pane_a, [], cx);
        assert_item_labels(&pane_b, ["B!", "A*"], cx);
    }

    #[gpui::test]
    async fn test_drag_pinned_tab_throughout_entire_range_of_pinned_tabs_both_directions(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B, C and pin all
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        let item_b = add_labeled_item(&pane_a, "B", false, cx);
        let item_c = add_labeled_item(&pane_a, "C", false, cx);
        assert_item_labels(&pane_a, ["A", "B", "C*"], cx);

        pane_a.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);

            let ix = pane.index_for_item_id(item_c.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane_a, ["A!", "B!", "C*!"], cx);

        // Move A to right of B
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 1, window, cx);
        });

        // A should be after B and all are pinned
        assert_item_labels(&pane_a, ["B!", "A*!", "C!"], cx);

        // Move A to right of C
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 1,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 2, window, cx);
        });

        // A should be after C and all are pinned
        assert_item_labels(&pane_a, ["B!", "C!", "A*!"], cx);

        // Move A to left of C
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 2,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 1, window, cx);
        });

        // A should be before C and all are pinned
        assert_item_labels(&pane_a, ["B!", "A*!", "C!"], cx);

        // Move A to left of B
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 1,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // A should be before B and all are pinned
        assert_item_labels(&pane_a, ["A*!", "B!", "C!"], cx);
    }

    #[gpui::test]
    async fn test_drag_first_tab_to_last_position(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B, C
        let item_a = add_labeled_item(&pane_a, "A", false, cx);
        add_labeled_item(&pane_a, "B", false, cx);
        add_labeled_item(&pane_a, "C", false, cx);
        assert_item_labels(&pane_a, ["A", "B", "C*"], cx);

        // Move A to the end
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_a.boxed_clone(),
                ix: 0,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 2, window, cx);
        });

        // A should be at the end
        assert_item_labels(&pane_a, ["B", "C", "A*"], cx);
    }

    #[gpui::test]
    async fn test_drag_last_tab_to_first_position(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane_a = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // Add A, B, C
        add_labeled_item(&pane_a, "A", false, cx);
        add_labeled_item(&pane_a, "B", false, cx);
        let item_c = add_labeled_item(&pane_a, "C", false, cx);
        assert_item_labels(&pane_a, ["A", "B", "C*"], cx);

        // Move C to the beginning
        pane_a.update_in(cx, |pane, window, cx| {
            let dragged_tab = DraggedTab {
                pane: pane_a.clone(),
                item: item_c.boxed_clone(),
                ix: 2,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged_tab, 0, window, cx);
        });

        // C should be at the beginning
        assert_item_labels(&pane_a, ["C*", "A", "B"], cx);
    }

    #[gpui::test]
    async fn test_add_item_with_new_item(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // 1. Add with a destination index
        //   a. Add before the active item
        set_labeled_items(&pane, ["A", "B*", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label("D"))),
                false,
                false,
                Some(0),
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["D*", "A", "B", "C"], cx);

        //   b. Add after the active item
        set_labeled_items(&pane, ["A", "B*", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label("D"))),
                false,
                false,
                Some(2),
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["A", "B", "D*", "C"], cx);

        //   c. Add at the end of the item list (including off the length)
        set_labeled_items(&pane, ["A", "B*", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label("D"))),
                false,
                false,
                Some(5),
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        // 2. Add without a destination index
        //   a. Add with active item at the start of the item list
        set_labeled_items(&pane, ["A*", "B", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label("D"))),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        set_labeled_items(&pane, ["A", "D*", "B", "C"], cx);

        //   b. Add with active item at the end of the item list
        set_labeled_items(&pane, ["A", "B", "C*"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label("D"))),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);
    }

    #[gpui::test]
    async fn test_add_item_with_existing_item(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // 1. Add with a destination index
        //   1a. Add before the active item
        let [_, _, _, d] = set_labeled_items(&pane, ["A", "B*", "C", "D"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(d, false, false, Some(0), window, cx);
        });
        assert_item_labels(&pane, ["D*", "A", "B", "C"], cx);

        //   1b. Add after the active item
        let [_, _, _, d] = set_labeled_items(&pane, ["A", "B*", "C", "D"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(d, false, false, Some(2), window, cx);
        });
        assert_item_labels(&pane, ["A", "B", "D*", "C"], cx);

        //   1c. Add at the end of the item list (including off the length)
        let [a, _, _, _] = set_labeled_items(&pane, ["A", "B*", "C", "D"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(a, false, false, Some(5), window, cx);
        });
        assert_item_labels(&pane, ["B", "C", "D", "A*"], cx);

        //   1d. Add same item to active index
        let [_, b, _] = set_labeled_items(&pane, ["A", "B*", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(b, false, false, Some(1), window, cx);
        });
        assert_item_labels(&pane, ["A", "B*", "C"], cx);

        //   1e. Add item to index after same item in last position
        let [_, _, c] = set_labeled_items(&pane, ["A", "B*", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(c, false, false, Some(2), window, cx);
        });
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        // 2. Add without a destination index
        //   2a. Add with active item at the start of the item list
        let [_, _, _, d] = set_labeled_items(&pane, ["A*", "B", "C", "D"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(d, false, false, None, window, cx);
        });
        assert_item_labels(&pane, ["A", "D*", "B", "C"], cx);

        //   2b. Add with active item at the end of the item list
        let [a, _, _, _] = set_labeled_items(&pane, ["A", "B", "C", "D*"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(a, false, false, None, window, cx);
        });
        assert_item_labels(&pane, ["B", "C", "D", "A*"], cx);

        //   2c. Add active item to active item at end of list
        let [_, _, c] = set_labeled_items(&pane, ["A", "B", "C*"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(c, false, false, None, window, cx);
        });
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        //   2d. Add active item to active item at start of list
        let [a, _, _] = set_labeled_items(&pane, ["A*", "B", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(a, false, false, None, window, cx);
        });
        assert_item_labels(&pane, ["A*", "B", "C"], cx);
    }

    #[gpui::test]
    async fn test_add_item_with_same_project_entries(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        // singleton view
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| {
                    TestItem::new(cx)
                        .with_singleton(true)
                        .with_label("buffer 1")
                        .with_project_items(&[TestProjectItem::new(1, "one.txt", cx)])
                })),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["buffer 1*"], cx);

        // new singleton view with the same project entry
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| {
                    TestItem::new(cx)
                        .with_singleton(true)
                        .with_label("buffer 1")
                        .with_project_items(&[TestProjectItem::new(1, "1.txt", cx)])
                })),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["buffer 1*"], cx);

        // new singleton view with different project entry
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| {
                    TestItem::new(cx)
                        .with_singleton(true)
                        .with_label("buffer 2")
                        .with_project_items(&[TestProjectItem::new(2, "2.txt", cx)])
                })),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["buffer 1", "buffer 2*"], cx);

        // new multibuffer view with the same project entry
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| {
                    TestItem::new(cx)
                        .with_singleton(false)
                        .with_label("multibuffer 1")
                        .with_project_items(&[TestProjectItem::new(1, "1.txt", cx)])
                })),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        assert_item_labels(&pane, ["buffer 1", "buffer 2", "multibuffer 1*"], cx);

        // another multibuffer view with the same project entry
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(
                Box::new(cx.new(|cx| {
                    TestItem::new(cx)
                        .with_singleton(false)
                        .with_label("multibuffer 1b")
                        .with_project_items(&[TestProjectItem::new(1, "1.txt", cx)])
                })),
                false,
                false,
                None,
                window,
                cx,
            );
        });
        assert_item_labels(
            &pane,
            ["buffer 1", "buffer 2", "multibuffer 1", "multibuffer 1b*"],
            cx,
        );
    }

    #[gpui::test]
    async fn test_remove_item_ordering_history(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", false, cx);
        add_labeled_item(&pane, "D", false, cx);
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(1, false, false, window, cx)
        });
        add_labeled_item(&pane, "1", false, cx);
        assert_item_labels(&pane, ["A", "B", "1*", "C", "D"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B*", "C", "D"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(3, false, false, window, cx)
        });
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B*", "C"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A*"], cx);
    }

    #[gpui::test]
    async fn test_remove_item_ordering_neighbour(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update_global::<SettingsStore, ()>(|s, cx| {
            s.update_user_settings::<ItemSettings>(cx, |s| {
                s.activate_on_close = Some(ActivateOnClose::Neighbour);
            });
        });
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", false, cx);
        add_labeled_item(&pane, "D", false, cx);
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(1, false, false, window, cx)
        });
        add_labeled_item(&pane, "1", false, cx);
        assert_item_labels(&pane, ["A", "B", "1*", "C", "D"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B", "C*", "D"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(3, false, false, window, cx)
        });
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A*"], cx);
    }

    #[gpui::test]
    async fn test_remove_item_ordering_left_neighbour(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update_global::<SettingsStore, ()>(|s, cx| {
            s.update_user_settings::<ItemSettings>(cx, |s| {
                s.activate_on_close = Some(ActivateOnClose::LeftNeighbour);
            });
        });
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", false, cx);
        add_labeled_item(&pane, "D", false, cx);
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(1, false, false, window, cx)
        });
        add_labeled_item(&pane, "1", false, cx);
        assert_item_labels(&pane, ["A", "B", "1*", "C", "D"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B*", "C", "D"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(3, false, false, window, cx)
        });
        assert_item_labels(&pane, ["A", "B", "C", "D*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.activate_item(0, false, false, window, cx)
        });
        assert_item_labels(&pane, ["A*", "B", "C"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["B*", "C"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["C*"], cx);
    }

    #[gpui::test]
    async fn test_close_inactive_items(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        let item_a = add_labeled_item(&pane, "A", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A*!"], cx);

        let item_b = add_labeled_item(&pane, "B", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_b.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
        });
        assert_item_labels(&pane, ["A!", "B*!"], cx);

        add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C*"], cx);

        add_labeled_item(&pane, "D", false, cx);
        add_labeled_item(&pane, "E", false, cx);
        assert_item_labels(&pane, ["A!", "B!", "C", "D", "E*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_inactive_items(
                &CloseInactiveItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A!", "B!", "E*"], cx);
    }

    #[gpui::test]
    async fn test_close_clean_items(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        add_labeled_item(&pane, "A", true, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", true, cx);
        add_labeled_item(&pane, "D", false, cx);
        add_labeled_item(&pane, "E", false, cx);
        assert_item_labels(&pane, ["A^", "B", "C^", "D", "E*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_clean_items(
                &CloseCleanItems {
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A^", "C*^"], cx);
    }

    #[gpui::test]
    async fn test_close_items_to_the_left(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_labeled_items(&pane, ["A", "B", "C*", "D", "E"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_items_to_the_left_by_id(
                None,
                &CloseItemsToTheLeft {
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["C*", "D", "E"], cx);
    }

    #[gpui::test]
    async fn test_close_items_to_the_right(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        set_labeled_items(&pane, ["A", "B", "C*", "D", "E"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_items_to_the_right_by_id(
                None,
                &CloseItemsToTheRight {
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A", "B", "C*"], cx);
    }

    #[gpui::test]
    async fn test_close_all_items(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        let item_a = add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, ["A*!"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.unpin_tab_at(ix, window, cx);
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        assert_item_labels(&pane, [], cx);

        add_labeled_item(&pane, "A", true, cx).update(cx, |item, cx| {
            item.project_items
                .push(TestProjectItem::new_dirty(1, "A.txt", cx))
        });
        add_labeled_item(&pane, "B", true, cx).update(cx, |item, cx| {
            item.project_items
                .push(TestProjectItem::new_dirty(2, "B.txt", cx))
        });
        add_labeled_item(&pane, "C", true, cx).update(cx, |item, cx| {
            item.project_items
                .push(TestProjectItem::new_dirty(3, "C.txt", cx))
        });
        assert_item_labels(&pane, ["A^", "B^", "C*^"], cx);

        let save = pane.update_in(cx, |pane, window, cx| {
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        });

        cx.executor().run_until_parked();
        cx.simulate_prompt_answer("Save all");
        save.await.unwrap();
        assert_item_labels(&pane, [], cx);

        add_labeled_item(&pane, "A", true, cx);
        add_labeled_item(&pane, "B", true, cx);
        add_labeled_item(&pane, "C", true, cx);
        assert_item_labels(&pane, ["A^", "B^", "C*^"], cx);
        let save = pane.update_in(cx, |pane, window, cx| {
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        });

        cx.executor().run_until_parked();
        cx.simulate_prompt_answer("Discard all");
        save.await.unwrap();
        assert_item_labels(&pane, [], cx);
    }

    #[gpui::test]
    async fn test_close_with_save_intent(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        let a = cx.update(|_, cx| TestProjectItem::new_dirty(1, "A.txt", cx));
        let b = cx.update(|_, cx| TestProjectItem::new_dirty(1, "B.txt", cx));
        let c = cx.update(|_, cx| TestProjectItem::new_dirty(1, "C.txt", cx));

        add_labeled_item(&pane, "AB", true, cx).update(cx, |item, _| {
            item.project_items.push(a.clone());
            item.project_items.push(b.clone());
        });
        add_labeled_item(&pane, "C", true, cx)
            .update(cx, |item, _| item.project_items.push(c.clone()));
        assert_item_labels(&pane, ["AB^", "C*^"], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: Some(SaveIntent::Save),
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        assert_item_labels(&pane, [], cx);
        cx.update(|_, cx| {
            assert!(!a.read(cx).is_dirty);
            assert!(!b.read(cx).is_dirty);
            assert!(!c.read(cx).is_dirty);
        });
    }

    #[gpui::test]
    async fn test_close_all_items_including_pinned(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());

        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        let item_a = add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", false, cx);
        assert_item_labels(&pane, ["A", "B", "C*"], cx);

        pane.update_in(cx, |pane, window, cx| {
            let ix = pane.index_for_item_id(item_a.item_id()).unwrap();
            pane.pin_tab_at(ix, window, cx);
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: None,
                    close_pinned: true,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
        assert_item_labels(&pane, [], cx);
    }

    #[gpui::test]
    async fn test_close_pinned_tab_with_non_pinned_in_same_pane(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        // Non-pinned tabs in same pane
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        add_labeled_item(&pane, "A", false, cx);
        add_labeled_item(&pane, "B", false, cx);
        add_labeled_item(&pane, "C", false, cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.pin_tab_at(0, window, cx);
        });
        set_labeled_items(&pane, ["A*", "B", "C"], cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
            .unwrap();
        });
        // Non-pinned tab should be active
        assert_item_labels(&pane, ["A!", "B*", "C"], cx);
    }

    #[gpui::test]
    async fn test_close_pinned_tab_with_non_pinned_in_different_pane(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        // No non-pinned tabs in same pane, non-pinned tabs in another pane
        let pane1 = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let pane2 = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(pane1.clone(), SplitDirection::Right, window, cx)
        });
        add_labeled_item(&pane1, "A", false, cx);
        pane1.update_in(cx, |pane, window, cx| {
            pane.pin_tab_at(0, window, cx);
        });
        set_labeled_items(&pane1, ["A*"], cx);
        add_labeled_item(&pane2, "B", false, cx);
        set_labeled_items(&pane2, ["B"], cx);
        pane1.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
            .unwrap();
        });
        //  Non-pinned tab of other pane should be active
        assert_item_labels(&pane2, ["B*"], cx);
    }

    #[gpui::test]
    async fn ensure_item_closing_actions_do_not_panic_when_no_items_exist(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        assert_item_labels(&pane, [], cx);

        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        pane.update_in(cx, |pane, window, cx| {
            pane.close_inactive_items(
                &CloseInactiveItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        pane.update_in(cx, |pane, window, cx| {
            pane.close_all_items(
                &CloseAllItems {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        pane.update_in(cx, |pane, window, cx| {
            pane.close_clean_items(
                &CloseCleanItems {
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        pane.update_in(cx, |pane, window, cx| {
            pane.close_items_to_the_right_by_id(
                None,
                &CloseItemsToTheRight {
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();

        pane.update_in(cx, |pane, window, cx| {
            pane.close_items_to_the_left_by_id(
                None,
                &CloseItemsToTheLeft {
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .unwrap();
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(LoadThemes::JustBase, cx);
            crate::init_settings(cx);
            Project::init_settings(cx);
        });
    }

    fn set_max_tabs(cx: &mut TestAppContext, value: Option<usize>) {
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<WorkspaceSettings>(cx, |settings| {
                settings.max_tabs = value.map(|v| NonZero::new(v).unwrap())
            });
        });
    }

    fn add_labeled_item(
        pane: &Entity<Pane>,
        label: &str,
        is_dirty: bool,
        cx: &mut VisualTestContext,
    ) -> Box<Entity<TestItem>> {
        pane.update_in(cx, |pane, window, cx| {
            let labeled_item =
                Box::new(cx.new(|cx| TestItem::new(cx).with_label(label).with_dirty(is_dirty)));
            pane.add_item(labeled_item.clone(), false, false, None, window, cx);
            labeled_item
        })
    }

    fn set_labeled_items<const COUNT: usize>(
        pane: &Entity<Pane>,
        labels: [&str; COUNT],
        cx: &mut VisualTestContext,
    ) -> [Box<Entity<TestItem>>; COUNT] {
        pane.update_in(cx, |pane, window, cx| {
            pane.items.clear();
            let mut active_item_index = 0;

            let mut index = 0;
            let items = labels.map(|mut label| {
                if label.ends_with('*') {
                    label = label.trim_end_matches('*');
                    active_item_index = index;
                }

                let labeled_item = Box::new(cx.new(|cx| TestItem::new(cx).with_label(label)));
                pane.add_item(labeled_item.clone(), false, false, None, window, cx);
                index += 1;
                labeled_item
            });

            pane.activate_item(active_item_index, false, false, window, cx);

            items
        })
    }

    // Assert the item label, with the active item label suffixed with a '*'
    #[track_caller]
    fn assert_item_labels<const COUNT: usize>(
        pane: &Entity<Pane>,
        expected_states: [&str; COUNT],
        cx: &mut VisualTestContext,
    ) {
        let actual_states = pane.update(cx, |pane, cx| {
            pane.items
                .iter()
                .enumerate()
                .map(|(ix, item)| {
                    let mut state = item
                        .to_any()
                        .downcast::<TestItem>()
                        .unwrap()
                        .read(cx)
                        .label
                        .clone();
                    if ix == pane.active_item_index {
                        state.push('*');
                    }
                    if item.is_dirty(cx) {
                        state.push('^');
                    }
                    if pane.is_tab_pinned(ix) {
                        state.push('!');
                    }
                    state
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            actual_states, expected_states,
            "pane items do not match expectation"
        );
    }
}
