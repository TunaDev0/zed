use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, Pixels, SharedString, WeakEntity, Window,
    actions, div, px,
    prelude::*,
};
use menu::{SelectNext, SelectPrevious};
use project::Project;
use serde::Deserialize;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader, Write};
use ui::{
    Button, ButtonCommon, ButtonSize, Color, IconName, Label, LabelSize, prelude::*,
};
use util::paths::PathStyle;
use workspace::{
    DraggedSelection, DraggedTab, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    claude_code_panel,
    [
        ToggleFocus,
        Toggle,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ClaudeCodePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<ClaudeCodePanel>(window, cx) {
                workspace.close_panel::<ClaudeCodePanel>(window, cx);
            }
        });
    })
    .detach();
}

#[derive(Clone, Deserialize)]
struct ClaudeEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    message: Option<ClaudeMessage>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    usage: Option<ClaudeUsageData>,
    #[serde(default)]
    model_usage: Option<serde_json::Value>,
}

#[derive(Clone, Deserialize, Default)]
struct ClaudeUsageData {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Clone)]
struct ClaudeUsage {
    total_cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
    context_window: u64,
    model: String,
}

#[derive(Clone, Deserialize)]
struct ClaudeMessage {
    content: Vec<ClaudeContent>,
}

#[derive(Clone, Deserialize)]
struct ClaudeContent {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default)]
    text: String,
}

const DAILY_BUDGET_USD: f64 = 10.0;

pub struct ClaudeCodePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    messages: Vec<ChatMessage>,
    input_buffer: String,
    is_loading: bool,
    session_id: Option<String>,
    show_commands: bool,
    filtered_commands: Vec<usize>,
    selected_command: usize,
    session_usage: ClaudeUsage,
}

const COMMANDS: &[(&str, &str)] = &[
    ("/clear", "Start new conversation"),
    ("/compact", "Compress conversation to save context"),
    ("/help", "Show available commands"),
    ("/model", "Switch AI model"),
    ("/effort", "Set reasoning effort (low/medium/high/xhigh/max)"),
    ("/cost", "Show token usage and cost"),
    ("/status", "Show session info"),
    ("/context", "Show context window usage"),
    ("/plan", "Plan before writing code"),
    ("/diff", "Show uncommitted changes"),
    ("/review", "Review current diff"),
    ("/explain", "Explain last response"),
    ("/retry", "Retry last response"),
    ("/undo", "Undo last file edit"),
    ("/resume", "Resume prior session"),
    ("/export", "Export conversation"),
];

#[derive(Clone)]
struct ChatMessage {
    role: SharedString,
    text: SharedString,
}

impl ClaudeCodePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let ws = workspace.clone();
        let panel = workspace
            .update_in(&mut cx, |_workspace, _window, cx| {
                let project = _workspace.project().clone();
                cx.new(|cx| Self {
                    focus_handle: cx.focus_handle(),
                    workspace: ws,
                    project,
                    messages: Vec::new(),
                    input_buffer: String::new(),
                    is_loading: false,
                    session_id: None,
                    show_commands: false,
                    filtered_commands: Vec::new(),
                    selected_command: 0,
                    session_usage: ClaudeUsage {
                        total_cost_usd: 0.0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read: 0,
                        cache_creation: 0,
                        context_window: 200_000,
                        model: String::new(),
                    },
                })
            })?;

        Ok(panel)
    }

    fn append_path_to_input(&mut self, path_str: &str, cx: &mut Context<Self>) {
        if !self.input_buffer.is_empty() && !self.input_buffer.ends_with(' ') {
            self.input_buffer.push(' ');
        }
        self.input_buffer.push_str(path_str);
        self.show_commands = false;
        cx.notify();
    }

    fn handle_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_strs: Vec<String> = paths
            .0
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        if path_strs.is_empty() {
            return;
        }
        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        for p in &path_strs {
            self.append_path_to_input(p, cx);
        }
    }

    fn handle_drop_tab(
        &mut self,
        tab: &DraggedTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = tab.pane.read(cx).item_for_index(tab.ix) else {
            return;
        };
        let Some(project_path) = item.project_path(cx) else {
            return;
        };
        let display_path = project_path.path.display(PathStyle::local()).to_string();
        if !self.focus_handle.is_focused(_window) {
            self.focus_handle.focus(_window, cx);
        }
        self.append_path_to_input(&display_path, cx);
    }

    fn handle_drop_selection(
        &mut self,
        selection: &DraggedSelection,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus_handle.is_focused(_window) {
            self.focus_handle.focus(_window, cx);
        }
        for entry in selection.items() {
            if let Some(project_path) = self.project.read(cx).path_for_entry(entry.entry_id, cx) {
                let display_path = project_path.path.display(PathStyle::local()).to_string();
                self.append_path_to_input(&display_path, cx);
            }
        }
    }

    fn handle_click_focus(
        &mut self,
        _: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
    }

    fn filter_commands(&self) -> Vec<usize> {
        let prefix = self.input_buffer.to_lowercase();
        COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| name.starts_with(&prefix))
            .map(|(i, _)| i)
            .collect()
    }

    fn handle_select_next(
        &mut self,
        _: &SelectNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.show_commands {
            return;
        }
        if self.selected_command + 1 < self.filtered_commands.len() {
            self.selected_command += 1;
            cx.notify();
        }
    }

    fn handle_select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.show_commands {
            return;
        }
        if self.selected_command > 0 {
            self.selected_command -= 1;
            cx.notify();
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_loading {
            return;
        }
        if event.keystroke.modifiers.control || event.keystroke.modifiers.platform {
            return;
        }

        if self.show_commands {
            match event.keystroke.key.as_str() {
                "enter" => {
                    if let Some(&idx) = self.filtered_commands.get(self.selected_command) {
                        let (cmd, _) = COMMANDS[idx];
                        self.input_buffer = cmd.to_string();
                        self.input_buffer.push(' ');
                        self.show_commands = false;
                    }
                    cx.notify();
                    return;
                }
                "escape" => {
                    self.show_commands = false;
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        if event.keystroke.key == "enter" && !self.input_buffer.is_empty() {
            let text = self.input_buffer.trim().to_string();
            if text.is_empty() {
                return;
            }
            self.input_buffer.clear();
            self.show_commands = false;
            self.messages.push(ChatMessage {
                role: "You".into(),
                text: text.clone().into(),
            });
            self.is_loading = true;
            cx.notify();
            self.spawn_claude_task(text, cx);
        } else if event.keystroke.key == "backspace" {
            self.input_buffer.pop();
            if self.input_buffer.is_empty() || !self.input_buffer.starts_with('/') {
                self.show_commands = false;
            } else {
                self.filtered_commands = self.filter_commands();
                self.selected_command = 0;
            }
            cx.notify();
        } else if let Some(ch) = &event.keystroke.key_char {
            if ch.len() == 1 {
                self.input_buffer.push_str(ch);
                if self.input_buffer == "/" {
                    self.show_commands = true;
                    self.filtered_commands = self.filter_commands();
                    self.selected_command = 0;
                } else if self.show_commands {
                    self.filtered_commands = self.filter_commands();
                    self.selected_command = 0;
                }
                cx.notify();
            }
        }
    }

    fn spawn_claude_task(&mut self, text: String, cx: &mut Context<Self>) {
        let session_id = self.session_id.clone();
        let workspace_path = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.project().read(cx).worktrees(cx).next().map(|wt| {
                    wt.read(cx).abs_path().to_path_buf()
                })
            })
            .ok()
            .flatten();

        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let (text_parts, sid, error, result_usage) = cx.background_spawn(async move {
                let mut current_sid: Option<String> = None;
                let mut text_parts: Vec<String> = Vec::new();
                let mut usage: ClaudeUsage = ClaudeUsage {
                    total_cost_usd: 0.0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_creation: 0,
                    context_window: 200_000,
                    model: String::new(),
                };

                let mut cmd = Command::new("claude");
                cmd.arg("-p")
                    .arg("--verbose")
                    .arg("--output-format")
                    .arg("stream-json");

                if let Some(ref sid) = session_id {
                    cmd.arg("--resume").arg(sid);
                }

                if let Some(ref path) = workspace_path {
                    cmd.current_dir(path);
                }

                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            text_parts,
                            current_sid,
                            Some(format!("Error: failed to spawn claude ({})", e)),
                            usage,
                        );
                    }
                };

                if let Some(ref mut stdin) = child.stdin {
                    let _ = writeln!(stdin, "{}", text);
                    let _ = stdin.flush();
                }
                drop(child.stdin.take());

                if let Some(stdout) = child.stdout.take() {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        match line {
                            Ok(line) => {
                                if line.is_empty() {
                                    continue;
                                }
                                if let Ok(event) =
                                    serde_json::from_str::<ClaudeEvent>(&line)
                                {
                                    match event.event_type.as_str() {
                                        "system" => {
                                            if event.session_id.is_some() {
                                                current_sid = event.session_id;
                                            }
                                        }
                                        "assistant" => {
                                            if let Some(msg) = event.message {
                                                for content in &msg.content {
                                                    if content.content_type == "text"
                                                        && !content.text.is_empty()
                                                    {
                                                        text_parts.push(content.text.clone());
                                                    }
                                                }
                                            }
                                        }
                                        "result" => {
                                            usage.total_cost_usd = event.total_cost_usd;
                                            if let Some(u) = &event.usage {
                                                usage.input_tokens = u.input_tokens;
                                                usage.output_tokens = u.output_tokens;
                                                usage.cache_read = u.cache_read_input_tokens;
                                                usage.cache_creation = u.cache_creation_input_tokens;
                                            }
                                            if let Some(mu) = &event.model_usage {
                                                if let Some(first_model) = mu.as_object().and_then(|obj| obj.values().next()) {
                                                    if let Some(cw) = first_model.get("contextWindow").and_then(|v| v.as_u64()) {
                                                        usage.context_window = cw;
                                                    }
                                                    if let Some(model_name) = mu.as_object().and_then(|obj| obj.keys().next()) {
                                                        usage.model = model_name.clone();
                                                    }
                                                }
                                            }
                                            if usage.total_cost_usd == 0.0 && usage.input_tokens == 0 {
                                                if event.session_id.is_some() {
                                                    current_sid = event.session_id;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Err(e) => {
                                return (
                                    text_parts,
                                    current_sid,
                                    Some(format!("Read error: {}", e)),
                                    usage,
                                );
                            }
                        }
                    }
                }

                let _ = child.wait();

                (text_parts, current_sid, None, usage)
            })
            .await;

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    if let Some(err) = error {
                        this.messages.push(ChatMessage {
                            role: "Error".into(),
                            text: err.into(),
                        });
                    } else {
                        if let Some(s) = sid {
                            this.session_id = Some(s);
                        }
                        let full_text = text_parts.concat();
                        if !full_text.is_empty() {
                            this.messages.push(ChatMessage {
                                role: "Claude".into(),
                                text: full_text.into(),
                            });
                        }
                        this.session_usage.total_cost_usd += result_usage.total_cost_usd;
                        this.session_usage.input_tokens += result_usage.input_tokens;
                        this.session_usage.output_tokens += result_usage.output_tokens;
                        this.session_usage.cache_read += result_usage.cache_read;
                        this.session_usage.cache_creation += result_usage.cache_creation;
                        if !result_usage.model.is_empty() {
                            this.session_usage.model = result_usage.model;
                        }
                        if result_usage.context_window > 0 {
                            this.session_usage.context_window = result_usage.context_window;
                        }
                    }
                    this.is_loading = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn send_message(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_buffer.trim().to_string();
        if text.is_empty() || self.is_loading {
            return;
        }

        self.input_buffer.clear();
        self.messages.push(ChatMessage {
            role: "You".into(),
            text: text.clone().into(),
        });
        self.is_loading = true;
        cx.notify();
        self.spawn_claude_task(text, cx);
    }

    fn format_usage_bar(&self) -> String {
        let cost = self.session_usage.total_cost_usd;
        let pct_of_daily = if DAILY_BUDGET_USD > 0.0 {
            ((cost / DAILY_BUDGET_USD) * 100.0).min(100.0)
        } else {
            0.0
        };
        format!("${:.3} | {:.0}% daily", cost, pct_of_daily)
    }
}

impl Focusable for ClaudeCodePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ClaudeCodePanel {}

impl Render for ClaudeCodePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let focused = self.focus_handle.is_focused(window);
        v_flex()
            .size_full()
            .bg(colors.panel_background)
            .relative()
            .track_focus(&self.focus_handle(cx))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_click_focus))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_action(cx.listener(Self::handle_select_next))
            .on_action(cx.listener(Self::handle_select_previous))
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(Label::new("Claude Code").size(LabelSize::Small)),
            )
            .child(
                div()
                    .id("claude-messages")
                    .flex_1()
                    .overflow_y_scroll()
                    .py_2()
                    .px_3()
                    .children(
                        if self.messages.is_empty() && !self.is_loading {
                            vec![
                                v_flex()
                                    .size_full()
                                    .justify_center()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Label::new("Ask Claude Code anything about your code")
                                            .color(Color::Muted)
                                            .size(LabelSize::Small),
                                    )
                                    .child(
                                        Label::new("Type a message below and press Enter to start")
                                            .color(Color::Muted)
                                            .size(LabelSize::Small),
                                    )
                                    .into_any_element(),
                            ]
                        } else {
                            self.messages.iter().map(|msg| {
                                let (role_label, bubble_bg) = match msg.role.as_ref() {
                                    "You" => ("You", colors.surface_background),
                                    "Error" => ("Error", colors.element_background),
                                    _ => ("Claude Code", colors.surface_background),
                                };
                                v_flex()
                                    .gap_1()
                                    .mb_3()
                                    .child(
                                        Label::new(role_label)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .bg(bubble_bg)
                                            .rounded_md()
                                            .child(
                                                Label::new(msg.text.clone())
                                                    .size(LabelSize::Small),
                                            ),
                                    )
                                    .into_any_element()
                            }).collect()
                        }
                    )
                    .when(self.is_loading, |this| {
                        if self.messages.is_empty() {
                            this
                        } else {
                            this.child(
                                h_flex()
                                    .gap_1()
                                    .mt_1()
                                    .child(
                                        Label::new("Thinking...")
                                            .color(Color::Muted)
                                            .size(LabelSize::Small),
                                    ),
                            )
                        }
                    }),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .bg(if focused { colors.title_bar_background } else { colors.panel_background })
                    .relative()
                    .when(self.show_commands, |this| {
                        this.child(
                            div()
                                .absolute()
                                .bottom_full()
                                .left_0()
                                .right_0()
                                .mb_1()
                                .mx_3()
                                .bg(colors.elevated_surface_background)
                                .border_1()
                                .border_color(colors.border)
                                .rounded_md()
                                .py_1()
                                .child(
                                    div()
                                        .flex_col()
                                        .children(
                                            self.filtered_commands.iter().enumerate().map(|(i, &cmd_idx)| {
                                                let (cmd, desc) = COMMANDS[cmd_idx];
                                                let is_selected = i == self.selected_command;
                                                h_flex()
                                                    .px_2()
                                                    .py_1()
                                                    .gap_2()
                                                    .cursor_pointer()
                                                    .when(is_selected, |this| this.bg(colors.element_selected))
                                                    .on_mouse_down(MouseButton::Left, {
                                                        let cmd = cmd.to_string();
                                                        cx.listener(move |this, _: &gpui::MouseDownEvent, _window, cx| {
                                                            this.input_buffer = cmd.clone();
                                                            this.input_buffer.push(' ');
                                                            this.show_commands = false;
                                                            cx.notify();
                                                        })
                                                    })
                                                    .child(
                                                        Label::new(cmd)
                                                            .size(LabelSize::Small)
                                                            .color(Color::Default),
                                                    )
                                                    .child(
                                                        Label::new(desc)
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .into_any_element()
                                            }),
                                        )
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("claude-input")
                                    .flex_1()
                                    .px_2()
                                    .py_1()
                                    .bg(colors.editor_background)
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if focused { colors.border } else { colors.border_variant })
                                    .text_sm()
                                    .child(
                                        h_flex()
                                            .gap_0()
                                            .child(
                                                if self.input_buffer.is_empty() {
                                                    Label::new("Type a message...")
                                                        .color(Color::Muted)
                                                        .size(LabelSize::Small)
                                                        .into_any_element()
                                                } else {
                                                    Label::new(self.input_buffer.clone())
                                                        .size(LabelSize::Small)
                                                        .into_any_element()
                                                },
                                            )
                                            .when(focused, |this| {
                                                this.child(
                                                    div()
                                                        .w(px(1.5))
                                                        .h(px(14.0))
                                                        .bg(if self.input_buffer.is_empty() { colors.text.alpha(0.3) } else { colors.text }),
                                                )
                                            }),
                                    ),
                            )
                            .child(
                                Button::new("send-btn", "Send")
                                    .size(ButtonSize::Compact)
                                    .disabled(self.input_buffer.trim().is_empty() || self.is_loading)
                                    .on_click(cx.listener(Self::send_message)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .bg(colors.title_bar_background)
                    .child(
                        Label::new(self.format_usage_bar())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(6.0))
                            .bg(colors.element_background)
                            .rounded_sm()
                            .overflow_hidden()
                    .child({
                        let pct = if DAILY_BUDGET_USD > 0.0 {
                            ((self.session_usage.total_cost_usd / DAILY_BUDGET_USD) * 100.0).min(100.0) as f32 / 100.0
                        } else {
                            0.0
                        };
                        let bar_color = if pct > 0.8 {
                            cx.theme().status().error_background
                        } else {
                            colors.element_selected
                        };
                        div()
                            .w(px(200.0 * pct))
                            .h_full()
                            .bg(bar_color)
                    }),
                    ),
            )
            .child(
                div()
                    .invisible()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .bg(colors.drop_target_background)
                    .drag_over::<ExternalPaths>(|style, _, _, _| style.visible())
                    .drag_over::<DraggedTab>(|style, _, _, _| style.visible())
                    .drag_over::<DraggedSelection>(|style, _, _, _| style.visible())
                    .on_drop(cx.listener(Self::handle_drop))
                    .on_drop(cx.listener(Self::handle_drop_tab))
                    .on_drop(cx.listener(Self::handle_drop_selection)),
            )
    }
}

impl Panel for ClaudeCodePanel {
    fn persistent_name() -> &'static str {
        "ClaudeCodePanel"
    }

    fn panel_key() -> &'static str {
        "ClaudeCodePanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(400.0)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Claude Code")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}
