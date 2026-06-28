use crate::git_graph::format_timestamp;

use git::repository::{InitialGraphCommitData, LogSource, LogOrder};
use git::Oid;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, ClipboardItem, Entity, EventEmitter,
    FocusHandle, Focusable, MouseButton, SharedString, UniformListScrollHandle, WeakEntity, Window,
    actions, div, px, uniform_list, prelude::*,
};
use project::git_store::{
    CommitDataState, GitGraphEvent, GitStore, GitStoreEvent,
    GraphDataResponse, Repository, RepositoryEvent,
};
use std::sync::Arc;
use ui::{
    Chip, ContextMenu, HighlightedLabel, IconName, Label, LabelSize, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    git_graph_panel,
    [
        ToggleFocus,
        Toggle,
    ]
);

const ROW_VERTICAL_PADDING: Pixels = px(4.0);

pub struct GitGraphPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    commits: Vec<Arc<InitialGraphCommitData>>,
    selected_entry_idx: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    context_menu: Option<Entity<ContextMenu>>,
}

impl GitGraphPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let panel = workspace.update_in(&mut cx, |workspace, _window, cx| {
            let project = workspace.project().clone();
            let git_store = project.read(cx).git_store().clone();

            cx.new(|cx| {
                Self {
                    focus_handle: cx.focus_handle(),
                    workspace: workspace.weak_handle(),
                    git_store: git_store.clone(),
                    commits: Vec::new(),
                    selected_entry_idx: None,
                    scroll_handle: UniformListScrollHandle::new(),
                    context_menu: None,
                }
            })
        })?;

        panel.update(&mut cx, |panel, cx| {
            panel.subscribe(cx);
            panel.fetch_initial_graph_data(cx);
        });

        Ok(panel)
    }

    fn subscribe(&mut self, cx: &mut Context<Self>) {
        let git_store = self.git_store.clone();
        cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(_updated_repo_id, repo_event, _) => {
                if let Some(repository) = this.get_repository(cx) {
                    this.handle_repository_event(repository, repo_event, cx);
                }
            }
            _ => {}
        })
        .detach();
    }

    fn handle_repository_event(
        &mut self,
        repository: Entity<Repository>,
        event: &RepositoryEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                self.commits.clear();
                self.selected_entry_idx = None;
                self.fetch_initial_graph_data(cx);
            }
            RepositoryEvent::GraphEvent((source, order), graph_event) => {
                match graph_event {
                    GitGraphEvent::CountUpdated(count) => {
                        let old_count = self.commits.len();
                        repository.update(cx, |repo, cx| {
                            let GraphDataResponse { commits, .. } = repo.graph_data(
                                source.clone(),
                                *order,
                                old_count..*count,
                                cx,
                            );
                            self.commits.extend(commits.iter().cloned());
                        });
                        cx.notify();
                    }
                    GitGraphEvent::LoadingError => {
                        cx.notify();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn fetch_initial_graph_data(&mut self, cx: &mut Context<Self>) {
        if let Some(repository) = self.get_repository(cx) {
            repository.update(cx, |repository, cx| {
                let GraphDataResponse { commits, .. } = repository.graph_data(
                    LogSource::All,
                    LogOrder::DateOrder,
                    0..usize::MAX,
                    cx,
                );
                self.commits.extend(commits.iter().cloned());
            });
            cx.notify();
        }
    }

    fn get_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.workspace
            .read_with(cx, |workspace, cx| {
                workspace.project().read(cx).active_repository(cx)
            })
            .ok()
            .flatten()
    }

    fn row_height(window: &Window, _cx: &App) -> Pixels {
        let rem_size = window.rem_size();
        let line_height = window.text_style().line_height_in_pixels(rem_size);
        let raw = line_height + ROW_VERTICAL_PADDING;
        let scale = window.scale_factor();
        (raw * scale).round() / scale
    }

    fn load_commit_data(
        &self,
        sha: Oid,
        cx: &mut App,
    ) -> Option<(SharedString, SharedString, i64)> {
        if let Some(repository) = self.get_repository(cx) {
            let data = repository.update(cx, |repo, cx| {
                repo.fetch_commit_data(sha, false, cx).clone()
            });
            match &data {
                CommitDataState::Loaded(commit_data) => {
                    Some((
                        commit_data.subject.clone(),
                        commit_data.author_name.clone(),
                        commit_data.commit_timestamp,
                    ))
                }
                _ => None,
            }
        } else {
            None
        }
    }

    fn open_commit_view(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(idx) else {
            return;
        };

        let sha = commit.sha.to_string();
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some(repo) = workspace.project().read(cx).active_repository(cx) {
                    crate::commit_view::CommitView::open(
                        sha,
                        repo.downgrade(),
                        workspace.weak_handle(),
                        None,
                        None,
                        window,
                        cx,
                    );
                }
            })
            .ok();
    }

    fn deploy_context_menu(
        &mut self,
        index: usize,
        _position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.commits.get(index) else {
            return;
        };
        let sha = commit.sha;
        let sha_short = sha.display_short();
        let ref_names: Vec<SharedString> = commit
            .ref_names
            .iter()
            .filter_map(|name| {
                let n = name.strip_prefix("tag: ").unwrap_or(name);
                if n.is_empty() || n == "HEAD" { None } else { Some(SharedString::from(n.to_string())) }
            })
            .collect();

        let header = if ref_names.is_empty() {
            format!("Commit {sha_short}")
        } else {
            format!("{} | {sha_short}", ref_names[0])
        };

        let panel = cx.entity();
        let menu = ContextMenu::build(window, cx, |menu, window, _| {
            let mut menu = menu
                .context(self.focus_handle.clone())
                .header(header)
                .entry(
                    "Copy SHA",
                    None,
                    window.handler_for(&panel, move |_panel, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                    }),
                );
            for ref_name in &ref_names {
                let name = ref_name.clone();
                menu = menu.entry(
                    format!("Copy \"{name}\""),
                    None,
                    move |_window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()));
                    },
                );
            }
            menu
        });
        self.context_menu = Some(menu);
    }
}

impl GitGraphPanel {
    pub fn register(workspace: &mut Workspace) {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<GitGraphPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<GitGraphPanel>(window, cx) {
                workspace.close_panel::<GitGraphPanel>(window, cx);
            }
        });
    }
}

impl Focusable for GitGraphPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for GitGraphPanel {}

impl Render for GitGraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let item_count = self.commits.len();
        let weak_self = cx.weak_entity();
        let row_height = Self::row_height(window, cx);

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(Label::new("Git Graph").size(LabelSize::Small))
                    .when(item_count > 0, |this| {
                        this.child(
                            Label::new(format!("{} commits", item_count))
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    }),
            )
            .child(
                div()
                    .size_full()
                    .child(
                        uniform_list(
                            "git-graph-commits",
                            item_count,
                            move |range, _window, cx| {
                                let panel = weak_self.upgrade();
                                let Some(panel) = panel else {
                                    return range.map(|_| div().into_any_element()).collect::<Vec<_>>();
                                };

                                range.map(|idx| {
                                    panel.update(cx, |panel, cx| {
                                        let Some(commit) = panel.commits.get(idx) else {
                                            return div().h(row_height).into_any_element();
                                        };

                                        let data = commit.sha;
                                        let commit_info = panel.load_commit_data(data, cx);

                                        let (subject, author_name, formatted_time) = match &commit_info {
                                            Some((s, a, t)) => {
                                                (s.clone(), a.clone(), format_timestamp(*t))
                                            }
                                            None => ("Loading…".into(), "".into(), String::new()),
                                        };

                                        let short_sha = commit.sha.display_short();
                                        let is_selected = panel.selected_entry_idx == Some(idx);
                                        let accent_color = cx.theme().accents()
                                            .0.first().copied().unwrap_or_default();

                                        div()
                                            .id(("commit-row", idx))
                                            .h(row_height)
                                            .px_2()
                                            .cursor_pointer()
                                            .when(is_selected, |this| {
                                                this.bg(cx.theme().colors().element_selected)
                                            })
                                            .hover(|style| style.bg(cx.theme().colors().element_hover))
                                            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                                panel.selected_entry_idx = Some(idx);
                                                panel.open_commit_view(idx, window, cx);
                                            }))
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                cx.listener(move |panel, event: &gpui::MouseDownEvent, window, cx| {
                                                    panel.selected_entry_idx = Some(idx);
                                                    panel.deploy_context_menu(idx, event.position, window, cx);
                                                }),
                                            )
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .overflow_hidden()
                                                    .child(
                                                        h_flex()
                                                            .gap_1()
                                                            .children(commit.ref_names.iter().filter_map(|name| {
                                                                let ref_name = name.strip_prefix("tag: ").unwrap_or(name);
                                                                if ref_name.is_empty() || ref_name == "HEAD" {
                                                                    return None;
                                                                }
                                                                let chip = Chip::new(SharedString::from(ref_name.to_string()))
                                                                    .label_size(LabelSize::Small)
                                                                    .truncate()
                                                                    .bg_color(accent_color.opacity(0.08))
                                                                    .border_color(accent_color.opacity(0.25));
                                                                Some(chip.into_any_element())
                                                            }))
                                                            .child(
                                                                HighlightedLabel::from_ranges(subject, vec![])
                                                                    .truncate(),
                                                            ),
                                                    )
                                                    .child(
                                                        Label::new(formatted_time)
                                                            .color(Color::Muted)
                                                            .truncate(),
                                                    )
                                                    .child(
                                                        Label::new(author_name)
                                                            .color(Color::Muted)
                                                            .truncate(),
                                                    )
                                                    .child(
                                                        Label::new(short_sha)
                                                            .color(Color::Muted)
                                                            .truncate(),
                                                    ),
                                            )
                                            .into_any_element()
                                    })
                                }).collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&self.scroll_handle),
                    ),
            )
    }
}

impl Panel for GitGraphPanel {
    fn persistent_name() -> &'static str {
        "GitGraphPanel"
    }

    fn panel_key() -> &'static str {
        "GitGraphPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.0)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitCommit)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Git Graph Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        2
    }
}
