use collections::HashMap;
use futures::{future::join_all, Future};
use serde::Deserialize;
use std::{fmt, mem, ops::Range, sync::Arc};

use editor::{scroll::Autoscroll, DisplayPoint, Editor, RowRangeExt};
use gpui::{
    actions, column_pixels, impl_actions, point, saturate, AppContext, AsyncAppContext, Bounds,
    Entity, EntityId, Global, HighlightStyle, Hsla, KeystrokeEvent, Model, ModelContext, Point,
    Subscription, View, ViewContext, WeakView,
};
use multi_buffer::MultiBufferPoint;
use search::{get_word_task, search_multipane, search_window, word_starts};
use settings::Settings;
use text::{Bias, SelectionGoal};
use theme::ThemeSettings;
use ui::{Context, Pixels, VisualContext, WindowContext};
use workspace::Workspace;

use crate::{
    editor_state::{EditorState, InputResult, OverlayState, Selection},
    perm::{TrieBuilder, TrimResult},
    util::{end_of_document, ranges, sort_matches_display, sort_matches_pixel, start_of_document},
};

// todo change the name of this
pub use crate::buffer_display::BufferDisplay;

mod buffer_display;
mod editor_events;
mod editor_state;
mod perm;
mod search;
mod util;

#[derive(Eq, PartialEq, Copy, Clone, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
enum Direction {
    #[default]
    BiDirectional,
    Forwards,
    Backwards,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct NChar {
    direction: Direction,
    n: u32,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Pattern(Direction);

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Word(Direction);

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct SubWord(Direction);

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct FullWord(Direction);

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Row(Direction);

impl_actions!(easy_motion, [NChar, Pattern, Word, SubWord, FullWord, Row]);

actions!(easy_motion, [Cancel, PatternSubmit]);

#[derive(Clone, Copy, Debug)]
enum WordType {
    Word,
    SubWord,
    FullWord,
}

pub struct EasyMotion {
    active_editor: Option<WeakView<Editor>>,
    editor_subscription: Option<Subscription>,
    dimming: bool,
    keys: Arc<str>,
    enabled: bool,
    editor_states: HashMap<EntityId, EditorState>,
    multipane_state: Option<EditorState>,
}

impl fmt::Debug for EasyMotion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EasyMotion")
            .field("active_editor", &self.active_editor)
            .field("dimming", &self.dimming)
            .field("enabled", &self.enabled)
            .field("editor_states(members)", &self.editor_states)
            .finish()
    }
}

struct GlobalEasyMotion(Model<EasyMotion>);

impl Global for GlobalEasyMotion {}

const DEFAULT_KEYS: &'static str = "asdghklqwertyuiopzxcvbnmfj";

pub fn init(cx: &mut AppContext) {
    let easy = cx.new_model({
        |_| EasyMotion {
            active_editor: None,
            editor_subscription: None,
            dimming: true,
            editor_states: HashMap::default(),
            enabled: true,
            keys: DEFAULT_KEYS.into(),
            multipane_state: None,
        }
    });
    EasyMotion::set_global(easy.clone(), cx);
    cx.observe_keystrokes(EasyMotion::observe_keystrokes)
        .detach();
    cx.observe_new_views(|workspace: &mut Workspace, cx| register(workspace, cx))
        .detach();

    editor_events::init(cx);
}

fn register(workspace: &mut Workspace, _: &ViewContext<Workspace>) {
    workspace.register_action(|workspace: &mut Workspace, action: &Word, cx| {
        EasyMotion::word(action, workspace, cx);
    });
    workspace.register_action(|workspace: &mut Workspace, action: &SubWord, cx| {
        EasyMotion::sub_word(action, workspace, cx);
    });
    workspace.register_action(|workspace: &mut Workspace, action: &FullWord, cx| {
        EasyMotion::full_word(action, workspace, cx);
    });

    workspace.register_action(|workspace: &mut Workspace, action: &NChar, cx| {
        EasyMotion::n_char(action, workspace, cx);
    });

    workspace.register_action(|workspace: &mut Workspace, action: &Pattern, cx| {
        EasyMotion::pattern(action, workspace, cx);
    });
    workspace.register_action(|workspace: &mut Workspace, _action: &PatternSubmit, cx| {
        EasyMotion::pattern_submit(workspace, cx);
    });

    workspace.register_action(|workspace: &mut Workspace, action: &Row, cx| {
        EasyMotion::row(action, workspace, cx);
    });

    workspace.register_action(|workspace: &mut Workspace, _: &Cancel, cx| {
        EasyMotion::cancel(workspace, cx);
    });
}

impl EasyMotion {
    pub fn update<F, S>(cx: &mut AppContext, f: F) -> Option<S>
    where
        F: FnOnce(&mut EasyMotion, &mut ModelContext<EasyMotion>) -> S,
    {
        EasyMotion::global(cx).map(|easy| easy.update(cx, f))
    }

    pub fn update_async<F, S>(cx: &mut AsyncAppContext, f: F) -> Option<S>
    where
        F: FnOnce(&mut EasyMotion, &mut ModelContext<EasyMotion>) -> S,
    {
        EasyMotion::global_async(cx).and_then(|easy| easy.update(cx, f).ok())
    }

    pub fn global_async(cx: &AsyncAppContext) -> Option<Model<Self>> {
        cx.try_read_global::<GlobalEasyMotion, _>(|global_easy, _cx| global_easy.0.clone())
    }

    pub fn global(cx: &AppContext) -> Option<Model<Self>> {
        cx.try_global::<GlobalEasyMotion>()
            .map(|model| model.0.clone())
    }

    pub fn set_global(easy: Model<Self>, cx: &mut AppContext) {
        cx.set_global(GlobalEasyMotion(easy));
    }

    pub fn read_with<S>(
        cx: &WindowContext,
        f: impl FnOnce(&EasyMotion, &AppContext) -> S,
    ) -> Option<S> {
        EasyMotion::global(cx).map(|easy| easy.read_with(cx, f))
    }

    pub fn read_with_async<F, S>(cx: &AsyncAppContext, f: F) -> Option<S>
    where
        F: FnOnce(&EasyMotion, &AppContext) -> S,
    {
        EasyMotion::global_async(cx).and_then(|easy| easy.read_with(cx, f).ok())
    }

    #[allow(dead_code)]
    fn update_active_editor<S>(
        cx: &mut ViewContext<Workspace>,
        update: impl FnOnce(&Editor, &ViewContext<Editor>) -> S,
    ) -> Option<S> {
        let editor = EasyMotion::read_with(cx, |easy, _cx| easy.active_editor.clone())
            .flatten()?
            .upgrade()?;
        Some(editor.update(cx, |editor, cx| update(editor, cx)))
    }

    fn activate_editor(&mut self, editor: View<Editor>) {
        self.active_editor = Some(editor.downgrade());
    }

    fn active_editor(cx: &WindowContext) -> Option<View<Editor>> {
        Self::read_with(cx, |easy, _| {
            easy.active_editor.as_ref().and_then(|weak| weak.upgrade())
        })
        .flatten()
    }

    fn active_editor_id(cx: &WindowContext) -> Option<EntityId> {
        Self::read_with(cx, |easy, _| {
            easy.active_editor.as_ref().map(|editor| editor.entity_id())
        })
        .flatten()
    }

    fn editors_with_bounding_boxes(
        workspace: &Workspace,
        cx: &mut WindowContext,
    ) -> Vec<(View<Editor>, Bounds<Pixels>)> {
        let panes = workspace.panes();
        panes
            .iter()
            .filter_map(|pane| {
                pane.update(cx, |pane, _cx| {
                    pane.active_item()
                        .and_then(|item| item.downcast::<Editor>())
                })
                .map(|editor| {
                    let bounding_box = workspace.center().bounding_box_for_pane(pane).unwrap();
                    (editor, bounding_box)
                })
            })
            .collect::<Vec<_>>()
    }

    pub(crate) fn latest_state(&self) -> &EditorState {
        if let Some(state) = self.multipane_state.as_ref() {
            return state;
        };
        self.active_editor
            .as_ref()
            .and_then(|editor| {
                let id = editor.entity_id();
                self.editor_states.get(&id)
            })
            .unwrap_or(&EditorState::None)
    }

    #[allow(dead_code)]
    fn new_state(&mut self) -> &EditorState {
        self.active_editor
            .as_ref()
            .map(|active_editor| {
                self.editor_states
                    .insert(active_editor.entity_id(), EditorState::default());
                self.editor_states.get(&active_editor.entity_id()).unwrap()
            })
            .unwrap()
    }

    fn clear_state(&mut self) {
        self.insert_state(EditorState::None);
    }

    fn insert_state(&mut self, state: EditorState) -> Option<()> {
        let active_editor = self.active_editor.as_ref()?;
        self.editor_states.insert(active_editor.entity_id(), state);
        Some(())
    }

    fn insert_multipane_state(new_state: EditorState, cx: &mut AppContext) -> Option<()> {
        Self::update(cx, move |easy, cx| {
            easy.multipane_state = Some(new_state);
            cx.notify();
        })
    }

    fn take_state(&mut self) -> Option<EditorState> {
        self.active_editor.as_ref().map(|active_editor| {
            self.editor_states
                .get_mut(&active_editor.entity_id())
                .map(|state| mem::take(state))
                .unwrap_or_default()
        })
    }

    fn word(action: &Word, workspace: &Workspace, cx: &mut WindowContext) {
        let Word(direction) = *action;
        // TODO other directions?
        // not sure if check for multiple editors is totally necessary
        if matches!(direction, Direction::BiDirectional)
            && workspace.is_split()
            && workspace_has_multiple_editors(workspace, cx)
        {
            EasyMotion::word_multipane(WordType::Word, workspace, cx);
        } else {
            EasyMotion::word_single_pane(WordType::Word, direction, cx);
        }
    }

    fn sub_word(action: &SubWord, workspace: &Workspace, cx: &mut WindowContext) {
        let SubWord(direction) = *action;
        // TODO other directions?
        // not sure if check for multiple editors is totally necessary
        if matches!(direction, Direction::BiDirectional)
            && workspace.is_split()
            && workspace_has_multiple_editors(workspace, cx)
        {
            todo!()
        } else {
            EasyMotion::word_single_pane(WordType::SubWord, direction, cx);
        }
    }

    fn full_word(action: &FullWord, workspace: &Workspace, cx: &mut WindowContext) {
        let FullWord(direction) = *action;
        // TODO other directions?
        // not sure if check for multiple editors is totally necessary
        if matches!(direction, Direction::BiDirectional)
            && workspace.is_split()
            && workspace_has_multiple_editors(workspace, cx)
        {
            todo!()
        } else {
            EasyMotion::word_single_pane(WordType::FullWord, direction, cx);
        }
    }

    fn word_single_pane(word_type: WordType, direction: Direction, cx: &mut WindowContext) {
        let Some(active_editor) = Self::active_editor(cx) else {
            return;
        };
        let entity_id = active_editor.entity_id();

        let new_state = active_editor.update(cx, |editor, cx| {
            let selections = editor.selections.newest_display(cx);
            let map = &editor.snapshot(cx).display_snapshot;
            let word_starts = word_starts(
                word_type,
                direction,
                map,
                &selections,
                &editor.text_layout_details(cx),
            );

            let new_state = Self::handle_new_matches(word_starts, direction, editor, cx);
            let ctx = new_state.keymap_context_layer();
            editor.set_keymap_context_layer::<Self>(ctx, cx);
            new_state
        });

        Self::update(cx, move |easy, cx| {
            easy.editor_states.insert(entity_id, new_state);
            cx.notify();
        });
    }

    fn word_multipane(word_type: WordType, workspace: &Workspace, cx: &mut WindowContext) {
        let Some(active_editor_id) = Self::active_editor_id(cx) else {
            return;
        };

        let editors = Self::editors_with_bounding_boxes(workspace, cx);

        // get words along with their display points within their editors
        // as well as their position for sorting purposes
        let (weak_editors, search_tasks): (Vec<_>, Vec<_>) = editors
            .iter()
            .map(|(editor, bounding_box)| {
                let entity_id = editor.entity_id();
                let task = editor.update(cx, |editor, cx| {
                    get_word_task(word_type, *bounding_box, entity_id, editor, cx)
                });
                (editor.downgrade(), task)
            })
            .unzip();

        // group each list of matches to a weak view of its corresponding editor
        let cursor = editors
            .into_iter()
            .find(|(editor_view, _)| editor_view.entity_id() == active_editor_id)
            .map(|(editor_view, bounding_box)| {
                editor_view.update(cx, |editor, cx| {
                    Self::get_cursor_pixels(&bounding_box, editor, cx)
                })
            })
            .unwrap();

        Self::process_match_tasks(cursor, weak_editors, search_tasks, cx);
        Self::insert_multipane_state(EditorState::PendingSearch, cx);
    }

    fn get_cursor_pixels(
        bounding_box: &Bounds<Pixels>,
        editor: &Editor,
        cx: &mut ViewContext<Editor>,
    ) -> Point<Pixels> {
        let style = cx.text_style();
        let scroll_position = editor.snapshot(cx).scroll_position();
        let font_size = style.font_size.to_pixels(cx.rem_size());
        let line_height = style.line_height_in_pixels(cx.rem_size());
        let font_id = cx.text_system().resolve_font(&style.font());
        let em_width = cx
            .text_system()
            .typographic_bounds(font_id, font_size, 'm')
            .unwrap()
            .size
            .width;
        let scroll_pixel_position = point(
            scroll_position.x * em_width,
            scroll_position.y * line_height,
        );

        let selections = editor.selections.newest_display(cx);
        let cursor = selections.start;
        let x = bounding_box.origin.x + 2.0 * column_pixels(&style, cursor.column() as usize, cx);
        let y = bounding_box.origin.y + cursor.row().0 as f32 * line_height;
        point(x, y) - scroll_pixel_position
    }

    fn simple_action(new_state: EditorState, workspace: &Workspace, cx: &mut WindowContext) {
        if workspace.is_split() && workspace_has_multiple_editors(workspace, cx) {
            let panes = workspace.panes();

            let editors = panes
                .into_iter()
                .filter_map(|pane| {
                    pane.update(cx, |pane, _cx| {
                        pane.active_item()
                            .and_then(|item| item.downcast::<Editor>())
                    })
                })
                .collect::<Vec<_>>();

            Self::update_editors(&new_state, true, editors.into_iter(), cx);

            Self::insert_multipane_state(new_state, cx);
        } else {
            let Some(active_editor) = Self::active_editor(cx) else {
                return;
            };
            let entity_id = active_editor.entity_id();

            let ctx = new_state.keymap_context_layer();
            active_editor.update(cx, |editor, cx| {
                editor.set_keymap_context_layer::<Self>(ctx, cx);
            });

            Self::update(cx, move |easy, cx| {
                easy.editor_states.insert(entity_id, new_state);
                cx.notify();
            });
        }
    }

    fn n_char(action: &NChar, workspace: &Workspace, cx: &mut WindowContext) {
        let n = action.n;
        let direction = action.direction;
        let new_state = EditorState::new_n_char(n as usize, direction);
        Self::simple_action(new_state, workspace, cx);
    }

    // there should probably be an editor view for this?
    // at the moment there's no way to backspace when entering a regex query
    fn pattern(action: &Pattern, workspace: &Workspace, cx: &mut WindowContext) {
        let Pattern(direction) = action;
        let new_state = EditorState::new_pattern(*direction);
        Self::simple_action(new_state, workspace, cx);
    }

    fn pattern_submit(workspace: &mut Workspace, cx: &mut WindowContext) {
        if let Some(state) =
            Self::update(cx, |easy, _| Some(easy.multipane_state.take()?)).flatten()
        {
            let EditorState::Pattern(pattern) = state else {
                return;
            };
            let Some(active_editor_id) = Self::active_editor_id(cx) else {
                return;
            };

            let editors = Self::editors_with_bounding_boxes(workspace, cx);
            let query = pattern.chars().to_string();
            let new_state =
                Self::show_trie_from_query_multipane(query, false, active_editor_id, editors, cx);
            Self::insert_multipane_state(new_state, cx);
        } else {
            let Some((state, editor)) = Self::update(cx, |easy, _| {
                let state = easy.take_state()?;
                let weak_editor = easy.active_editor.clone()?;
                let editor = weak_editor.upgrade()?;
                Some((state, editor))
            })
            .flatten() else {
                return;
            };
            if !state.easy_motion_controlled() {
                return;
            }

            let EditorState::Pattern(pattern) = state else {
                return;
            };
            let query = pattern.chars().to_string();
            let direction = pattern.direction();
            let new_state = editor.update(cx, |editor, cx| {
                Self::show_trie_from_query(query, false, direction, editor, cx)
            });

            let entity_id = editor.entity_id();
            Self::update(cx, move |easy, cx| {
                easy.editor_states.insert(entity_id, new_state);
                cx.notify();
            });
        };
    }

    fn row(action: &Row, workspace: &Workspace, cx: &mut WindowContext) {
        let Row(direction) = action;
        if matches!(direction, Direction::BiDirectional)
            && workspace.is_split()
            && workspace_has_multiple_editors(workspace, cx)
        {
            EasyMotion::row_multipane(workspace, cx);
        } else {
            EasyMotion::row_single_pane(*direction, cx);
        }
    }

    fn row_multipane(workspace: &Workspace, cx: &mut WindowContext) {}

    fn row_single_pane(direction: Direction, cx: &mut WindowContext) {
        let Some(active_editor) = Self::active_editor(cx) else {
            return;
        };
        let entity_id = active_editor.entity_id();

        let new_state = active_editor.update(cx, |editor, cx| {
            let matches = Self::row_starts(direction, editor, cx);
            let new_state = Self::handle_new_matches(matches, direction, editor, cx);
            let ctx = new_state.keymap_context_layer();
            editor.set_keymap_context_layer::<Self>(ctx, cx);
            new_state
        });

        Self::update(cx, move |easy, cx| {
            easy.editor_states.insert(entity_id, new_state);
            cx.notify();
        });
    }

    fn row_starts(
        direction: Direction,
        editor: &mut Editor,
        cx: &mut ViewContext<Editor>,
    ) -> Vec<DisplayPoint> {
        let selections = editor.selections.newest_display(cx);
        let snapshot = editor.snapshot(cx);
        let map = &snapshot.display_snapshot;
        let Range { start, end } =
            ranges(direction, map, &selections, &editor.text_layout_details(cx));
        snapshot
            .buffer_rows(start.row())
            .take((start.row()..end.row()).len())
            .flatten()
            .filter_map(|row| {
                if snapshot.is_line_folded(row) {
                    None
                } else {
                    Some(map.point_to_display_point(MultiBufferPoint::new(row.0, 0), Bias::Left))
                }
            })
            .collect::<Vec<_>>()
    }

    fn observe_keystrokes(keystroke_event: &KeystrokeEvent, cx: &mut WindowContext) {
        if keystroke_event.action.is_some() {
            return;
        } else if cx.has_pending_keystrokes() {
            return;
        }

        if let Some(state) = Self::update(cx, |easy, _| easy.multipane_state.take()).flatten() {
            Self::observe_keystrokes_impl_multipane(keystroke_event, state, cx)
        } else {
            Self::observe_keystrokes_impl(keystroke_event, cx);
        };
    }

    fn observe_keystrokes_impl(keystroke_event: &KeystrokeEvent, cx: &mut WindowContext) {
        let Some((state, weak_editor)) = Self::update(cx, |easy, _| {
            let state = easy.take_state()?;
            let weak_editor = easy.active_editor.clone()?;
            Some((state, weak_editor))
        })
        .flatten() else {
            return;
        };

        if !state.easy_motion_controlled() {
            return;
        }

        let editor = weak_editor.upgrade();
        let Some(editor) = editor else {
            return;
        };
        let entity_id = editor.entity_id();

        let keys = keystroke_event.keystroke.key.as_str();
        let new_state = editor.update(cx, |editor, cx| match state {
            EditorState::NCharInput(char_input) => {
                let direction = char_input.direction();
                let res = char_input.record_str(keys);
                match res {
                    InputResult::ShowTrie(query) => {
                        Self::show_trie_from_query(query, false, direction, editor, cx)
                    }
                    InputResult::Recording(n_char) => EditorState::NCharInput(n_char),
                }
            }
            EditorState::Selection(selection) => Self::handle_trim(selection, keys, editor, cx),
            EditorState::PendingSearch => EditorState::PendingSearch,
            EditorState::Pattern(pattern) => EditorState::Pattern(pattern.record_str(keys)),
            EditorState::None => EditorState::None,
        });

        Self::update(cx, move |easy, cx| {
            easy.editor_states.insert(entity_id, new_state);
            cx.notify();
        });
    }

    fn observe_keystrokes_impl_multipane(
        keystroke_event: &KeystrokeEvent,
        state: EditorState,
        cx: &mut WindowContext,
    ) {
        if !state.easy_motion_controlled() {
            return;
        }

        let keys = keystroke_event.keystroke.key.as_str();
        let new_state = match state {
            EditorState::NCharInput(char_input) => {
                let res = char_input.record_str(keys);
                match res {
                    InputResult::ShowTrie(query) => cx
                        .window_handle()
                        .downcast::<Workspace>()
                        .and_then(|handle| handle.root(cx).ok())
                        .map(|workspace_view| {
                            workspace_view.update(cx, |workspace, cx| {
                                let Some(active_editor_id) = Self::active_editor_id(cx) else {
                                    return EditorState::None;
                                };

                                let editors = Self::editors_with_bounding_boxes(workspace, cx);
                                Self::show_trie_from_query_multipane(
                                    query,
                                    false,
                                    active_editor_id,
                                    editors,
                                    cx,
                                )
                            })
                        })
                        .unwrap_or_default(),
                    // do nothing
                    InputResult::Recording(n_char) => EditorState::NCharInput(n_char),
                }
            }
            EditorState::Selection(selection) => cx
                .window_handle()
                .downcast::<Workspace>()
                .and_then(|handle| handle.root(cx).ok())
                .map(|workspace_view| {
                    workspace_view.update(cx, |workspace, cx| {
                        Self::handle_trim_multipane(selection, keys, workspace, cx)
                    })
                })
                .unwrap_or_default(),
            EditorState::PendingSearch => EditorState::PendingSearch,
            EditorState::Pattern(pattern) => EditorState::Pattern(pattern.record_str(keys)),
            EditorState::None => EditorState::None,
        };

        Self::insert_multipane_state(new_state, cx);
    }

    fn handle_trim(
        selection: Selection,
        keys: &str,
        editor: &mut Editor,
        cx: &mut ViewContext<Editor>,
    ) -> EditorState {
        let (selection, res) = selection.record_str(keys);
        match res {
            TrimResult::Found(overlay) => {
                editor.change_selections(Some(Autoscroll::fit()), cx, |selection| {
                    selection.move_cursors_with(|_, _, _| (overlay.point, SelectionGoal::None))
                });
                editor.clear_overlays::<Self>(cx);
                editor.clear_highlights::<Self>(cx);
                editor.remove_keymap_context_layer::<Self>(cx);
                EditorState::None
            }
            TrimResult::Changed => {
                let trie = selection.trie();
                editor.clear_overlays::<Self>(cx);
                Self::add_overlays(editor, trie.iter(), cx);
                EditorState::Selection(selection)
            }
            TrimResult::Err => {
                editor.clear_overlays::<Self>(cx);
                editor.clear_highlights::<Self>(cx);
                editor.remove_keymap_context_layer::<Self>(cx);
                EditorState::None
            }
            TrimResult::NoChange => EditorState::Selection(selection),
        }
    }

    fn handle_trim_multipane(
        selection: Selection,
        keys: &str,
        workspace: &mut Workspace,
        cx: &mut WindowContext,
    ) -> EditorState {
        let editors = active_editor_views(workspace, cx);
        let (selection, res) = selection.record_str(keys);
        match res {
            TrimResult::Found(overlay) => {
                let Some(editor) = editors
                    .iter()
                    .find(|editor| editor.entity_id() == overlay.editor_id)
                else {
                    return EditorState::None;
                };
                workspace.activate_item(editor, cx);
                editor.update(cx, |editor, cx| {
                    editor.change_selections(Some(Autoscroll::fit()), cx, |selection| {
                        selection.move_cursors_with(|_, _, _| (overlay.point, SelectionGoal::None))
                    });
                });
                for editor in editors {
                    editor.update(cx, |editor, cx| {
                        editor.clear_overlays::<Self>(cx);
                        editor.clear_highlights::<Self>(cx);
                        editor.remove_keymap_context_layer::<Self>(cx);
                    });
                }
                EditorState::None
            }
            TrimResult::Changed => {
                let trie = selection.trie();
                for editor in editors {
                    let iter = trie
                        .iter()
                        .filter(|(_, overlay)| overlay.editor_id == editor.entity_id());
                    editor.update(cx, |editor, cx| {
                        editor.clear_overlays::<Self>(cx);
                        Self::add_overlays(editor, iter, cx);
                    });
                }
                EditorState::Selection(selection)
            }
            TrimResult::Err => {
                for editor in editors {
                    editor.update(cx, |editor, cx| {
                        editor.clear_overlays::<Self>(cx);
                        editor.clear_highlights::<Self>(cx);
                        editor.remove_keymap_context_layer::<Self>(cx);
                    });
                }
                EditorState::None
            }
            TrimResult::NoChange => EditorState::Selection(selection),
        }
    }

    fn handle_new_matches(
        mut matches: Vec<DisplayPoint>,
        direction: Direction,
        editor: &mut Editor,
        cx: &mut ViewContext<Editor>,
    ) -> EditorState {
        let selections = editor.selections.newest_display(cx);
        let snapshot = editor.snapshot(cx);
        let map = &snapshot.display_snapshot;

        if matches.is_empty() {
            return EditorState::None;
        }
        sort_matches_display(&mut matches, &selections.start);

        let (keys, dimming) = Self::read_with(cx, |easy, _| (easy.keys.clone(), easy.dimming))
            .unwrap_or((DEFAULT_KEYS.into(), false));

        let (style_0, style_1, style_2) = get_highlights(cx);
        let trie =
            TrieBuilder::new(keys, matches.len()).populate_with(true, matches, |seq, point| {
                let style = match seq.len() {
                    0 | 1 => style_0,
                    2 => style_1,
                    3.. => style_2,
                };
                OverlayState {
                    style,
                    point,
                    editor_id: cx.entity_id(),
                }
            });
        Self::add_overlays(editor, trie.iter(), cx);

        if dimming {
            let start = match direction {
                Direction::BiDirectional | Direction::Backwards => DisplayPoint::zero(),
                Direction::Forwards => selections.start,
            };
            let end = match direction {
                Direction::BiDirectional | Direction::Forwards => end_of_document(map),
                Direction::Backwards => selections.end,
            };
            let anchor_start = map.display_point_to_anchor(start, Bias::Left);
            let anchor_end = map.display_point_to_anchor(end, Bias::Left);
            let highlight = HighlightStyle {
                fade_out: Some(0.7),
                ..Default::default()
            };
            editor.highlight_text::<Self>(vec![anchor_start..anchor_end], highlight, cx);
        }

        EditorState::new_selection(trie)
    }

    fn show_trie_from_query(
        query: String,
        is_regex: bool,
        direction: Direction,
        editor: &mut Editor,
        cx: &mut ViewContext<Editor>,
    ) -> EditorState {
        let task = search_window(query.as_str(), is_regex, direction, editor, cx);
        let Some(task) = task else {
            return EditorState::None;
        };

        cx.spawn(|editor, mut cx| async move {
            let entity_id = editor.entity_id();
            let Some(editor) = editor.upgrade() else {
                return;
            };

            let matches = task.await;
            let res = editor.update(&mut cx, move |editor, cx| {
                editor.clear_search_within_ranges(cx);
                let new_state = Self::handle_new_matches(matches, direction, editor, cx);
                let ctx = new_state.keymap_context_layer();
                editor.set_keymap_context_layer::<Self>(ctx, cx);
                new_state
            });
            match res {
                Ok(state) => {
                    Self::update_async(&mut cx, move |easy, cx| {
                        easy.editor_states.insert(entity_id, state);
                        cx.notify();
                    });
                }
                Err(err) => {
                    dbg!(err);
                }
            }
        })
        .detach();

        EditorState::PendingSearch
    }

    fn show_trie_from_query_multipane(
        query: String,
        is_regex: bool,
        active_editor_id: EntityId,
        editors: Vec<(View<Editor>, Bounds<Pixels>)>,
        cx: &mut WindowContext,
    ) -> EditorState {
        // group each list of matches to a weak view of its corresponding editor
        let (weak_editors, search_tasks): (Vec<_>, Vec<_>) = editors
            .iter()
            .filter_map(|(editor, bounding_box)| {
                let entity_id = editor.entity_id();
                if let Some(search_res) = editor.update(cx, |editor, cx| {
                    search_multipane(
                        query.as_str(),
                        is_regex,
                        *bounding_box,
                        entity_id,
                        editor,
                        cx,
                    )
                }) {
                    Some((editor.downgrade(), search_res))
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.clear_search_within_ranges(cx);
                        editor.clear_highlights::<Self>(cx);
                        editor.remove_keymap_context_layer::<Self>(cx);
                    });
                    None
                }
            })
            .unzip();
        if search_tasks.len() == 0 {
            return EditorState::None;
        }

        // group each list of matches to a weak view of its corresponding editor
        let cursor = editors
            .into_iter()
            .find(|(editor_view, _)| editor_view.entity_id() == active_editor_id)
            .map(|(editor_view, bounding_box)| {
                editor_view.update(cx, |editor, cx| {
                    Self::get_cursor_pixels(&bounding_box, editor, cx)
                })
            })
            .unwrap();

        Self::process_match_tasks(cursor, weak_editors, search_tasks, cx);
        EditorState::PendingSearch
    }

    fn process_match_tasks(
        cursor: Point<Pixels>,
        weak_editors: Vec<WeakView<Editor>>,
        search_tasks: Vec<
            impl Future<Output = Vec<(DisplayPoint, EntityId, Point<Pixels>)>> + 'static + Send,
        >,
        cx: &mut WindowContext,
    ) {
        let sort_task = cx.background_executor().spawn(async move {
            let cursor = cursor;
            let mut search_matches = join_all(search_tasks)
                .await
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            if !search_matches.is_empty() {
                sort_matches_pixel(&mut search_matches, &cursor);
            }
            search_matches
        });
        cx.spawn(move |mut cx| async move {
            let cx = &mut cx;
            let editors = weak_editors
                .into_iter()
                .filter_map(|editor| editor.upgrade());

            let search_matches = sort_task.await;
            let len = search_matches.len();
            if len == 0 {
                Self::update_editors(&EditorState::None, false, editors, cx);
                return;
            }

            let (keys, dimming) =
                Self::read_with_async(&cx, |easy, _| (easy.keys.clone(), easy.dimming))
                    .unwrap_or((DEFAULT_KEYS.into(), false));

            let matches = search_matches.into_iter().map(|(point, id, _)| (point, id));

            let (style_0, style_1, style_2) = get_highlights_async(&cx);
            let trie = TrieBuilder::new(keys, len).populate_with(true, matches, |seq, point| {
                let style = match seq.len() {
                    0 | 1 => style_0,
                    2 => style_1,
                    3.. => style_2,
                };
                OverlayState {
                    style,
                    point: point.0,
                    editor_id: point.1,
                }
            });

            let new_state = EditorState::new_selection(trie);
            Self::update_editors(&new_state, dimming, editors, cx);

            Self::update_async(cx, move |easy, cx| {
                easy.multipane_state = Some(new_state);
                cx.notify();
            });
        })
        .detach();
    }

    fn update_editors(
        state: &EditorState,
        dimming: bool,
        editors: impl Iterator<Item = View<Editor>>,
        cx: &mut impl VisualContext,
    ) {
        // filter trie entries by editor and add overlays
        let ctx = state.keymap_context_layer();
        match state {
            EditorState::None => {
                for editor in editors {
                    editor.update(cx, |editor, cx| {
                        editor.clear_search_within_ranges(cx);
                        editor.clear_highlights::<Self>(cx);
                        editor.remove_keymap_context_layer::<Self>(cx);
                    });
                }
            }
            EditorState::Selection(selection) => {
                for editor in editors {
                    let trie = selection.trie();
                    let trie_iter = trie
                        .iter()
                        .filter(|(_seq, overlay)| overlay.editor_id == editor.entity_id());

                    editor.update(cx, |editor, cx| {
                        editor.set_keymap_context_layer::<Self>(ctx.clone(), cx);
                        editor.clear_search_within_ranges(cx);

                        Self::add_overlays(editor, trie_iter, cx);

                        if !dimming {
                            return;
                        }

                        let map = &editor.snapshot(cx).display_snapshot;
                        let start = start_of_document(map);
                        let end = end_of_document(map);
                        let anchor_start = map.display_point_to_anchor(start, Bias::Left);
                        let anchor_end = map.display_point_to_anchor(end, Bias::Left);
                        let highlight = HighlightStyle {
                            fade_out: Some(0.7),
                            ..Default::default()
                        };
                        editor.highlight_text::<Self>(
                            vec![anchor_start..anchor_end],
                            highlight,
                            cx,
                        );
                    });
                }
            }
            EditorState::NCharInput(_) | EditorState::Pattern(_) => {
                for editor in editors {
                    editor.update(cx, |editor, cx| {
                        editor.clear_highlights::<Self>(cx);
                        editor.set_keymap_context_layer::<Self>(ctx.clone(), cx);
                    });
                }
            }
            EditorState::PendingSearch => {}
        }
    }

    fn cancel(workspace: &Workspace, cx: &mut WindowContext) {
        let editor = Self::update(cx, |easy, _| {
            if let Some(state) = easy.multipane_state.as_mut() {
                state.clear();
                None
            } else {
                easy.clear_state();
                easy.active_editor.clone()
            }
        })
        .flatten();

        if workspace.panes().len() > 1 {
            let editors = active_editor_views(workspace, cx);
            for editor in editors {
                editor.update(cx, |editor, cx| {
                    editor.clear_overlays::<Self>(cx);
                    editor.clear_highlights::<Self>(cx);
                    editor.remove_keymap_context_layer::<Self>(cx);
                });
            }
        } else if let Some(editor) = editor.and_then(|editor| editor.upgrade()) {
            editor.update(cx, |editor, cx| {
                editor.clear_overlays::<Self>(cx);
                editor.clear_highlights::<Self>(cx);
                editor.remove_keymap_context_layer::<Self>(cx);
            });
        }
    }

    fn add_overlays<'a>(
        editor: &mut Editor,
        trie_iter: impl Iterator<Item = (String, &'a OverlayState)>,
        cx: &mut ViewContext<Editor>,
    ) {
        for (seq, overlay) in trie_iter {
            let mut highlights = vec![(0..1, overlay.style)];
            if seq.len() > 1 {
                highlights.push((
                    1..seq.len(),
                    HighlightStyle {
                        fade_out: Some(0.3),
                        ..overlay.style
                    },
                ));
            }
            editor.add_overlay::<Self>(seq, overlay.point, 0.0, highlights, cx);
        }
    }
}

fn workspace_has_multiple_editors(workspace: &Workspace, cx: &WindowContext) -> bool {
    let panes = workspace.panes();
    panes
        .iter()
        .filter(|pane| {
            pane.read(cx)
                .active_item()
                .and_then(|item| item.downcast::<Editor>())
                .is_some()
        })
        .take(2)
        .count()
        == 2
}

fn active_editor_views(workspace: &Workspace, cx: &WindowContext) -> Vec<View<Editor>> {
    let panes = workspace.panes();
    panes
        .iter()
        .filter_map(|pane| {
            pane.read(cx)
                .active_item()
                .and_then(|item| item.downcast::<Editor>())
        })
        .collect()
}

fn get_highlights(cx: &AppContext) -> (HighlightStyle, HighlightStyle, HighlightStyle) {
    let theme = &ThemeSettings::get_global(cx).active_theme;
    let players = &theme.players().0;
    let bg = theme.colors().background;
    let style_0 = HighlightStyle {
        color: Some(saturate(players[0].cursor, 1.0)),
        background_color: Some(bg),
        ..HighlightStyle::default()
    };
    let style_1 = HighlightStyle {
        color: Some(saturate(players[2].cursor, 1.0)),
        background_color: Some(bg),
        ..HighlightStyle::default()
    };
    let style_2 = HighlightStyle {
        color: Some(saturate(players[3].cursor, 1.0)),
        background_color: Some(bg),
        ..HighlightStyle::default()
    };
    (style_0, style_1, style_2)
}

fn get_highlights_async(cx: &AsyncAppContext) -> (HighlightStyle, HighlightStyle, HighlightStyle) {
    let (bg, color_0, color_1, color_2) = ThemeSettings::try_read_global(cx, |theme| {
        let theme = theme.active_theme.clone();
        let players = &theme.players().0;
        let bg = theme.colors().background;
        let color_0 = saturate(players[0].cursor, 1.0);
        let color_1 = saturate(players[2].cursor, 1.0);
        let color_2 = saturate(players[3].cursor, 1.0);
        (bg, color_0, color_1, color_2)
    })
    .unwrap_or((Hsla::white(), Hsla::red(), Hsla::green(), Hsla::blue()));

    let style_0 = HighlightStyle {
        color: Some(color_0),
        background_color: Some(bg),
        ..HighlightStyle::default()
    };
    let style_1 = HighlightStyle {
        color: Some(color_1),
        background_color: Some(bg),
        ..HighlightStyle::default()
    };
    let style_2 = HighlightStyle {
        color: Some(color_2),
        background_color: Some(bg),
        ..HighlightStyle::default()
    };
    (style_0, style_1, style_2)
}
