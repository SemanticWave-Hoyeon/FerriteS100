//! Chat window for locally installed LLM command-line clients (Codex CLI, Claude Code).
//! The host owns detection and the client processes; this module only draws and records
//! requests, so the frame never waits on a client.
use crate::ui_chrome::Theme;

/// Messages kept in the window; older ones are dropped first.
pub const MAX_MESSAGES: usize = 200;
/// Characters kept per message; the host truncates longer replies.
pub const MAX_MESSAGE_CHARS: usize = 64 * 1024;
/// Longest prompt the window accepts.
pub const MAX_INPUT_CHARS: usize = 16 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provider {
    /// Stable identifier ("codex" or "claude").
    pub id: String,
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// Status lines from the window itself (tool use, errors, provider changes).
    Notice,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    Detecting,
    Running,
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub open: bool,
    /// Detected clients; empty until detection has run.
    pub providers: Vec<Provider>,
    /// Detection has completed at least once for the current service session.
    pub detected: bool,
    pub selected: Option<String>,
    pub messages: Vec<Message>,
    pub input: String,
    pub status: Status,
    /// Whether the next turn will be given the S-100 dataset tools, and why not.
    pub tools_attached: bool,
    pub tools_note: String,
    // Requests from the window, consumed by the host.
    pub send: Option<String>,
    pub stop: bool,
    pub new_conversation: bool,
    pub redetect: bool,
    /// Resend the last user message after an error.
    pub retry: bool,
    /// "Show on Chart" request: [latitude, longitude].
    pub show_position: Option<[f64; 2]>,
    /// egui time when the current run started (window-local bookkeeping).
    run_started: Option<f64>,
    /// Window rectangle in points [left, top, right, bottom] from the last
    /// frame, so the host can show a position beside the window.
    pub window_rect: Option<[f32; 4]>,
}

impl State {
    pub fn running(&self) -> bool {
        self.status == Status::Running
    }
    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        let mut text: String = text.into();
        truncate_chars(&mut text, MAX_MESSAGE_CHARS);
        // Progress notices that only change a counter ("Reconnecting… 2/5")
        // replace the previous one instead of stacking up.
        if role == Role::Notice {
            if let Some(last) = self.messages.last_mut() {
                if last.role == Role::Notice && same_progress(&last.text, &text) {
                    last.text = text;
                    return;
                }
            }
        }
        self.messages.push(Message { role, text });
        let excess = self.messages.len().saturating_sub(MAX_MESSAGES);
        self.messages.drain(..excess);
    }
    /// Appends streamed assistant text to the current reply, starting one if needed.
    pub fn append_assistant(&mut self, text: &str) {
        match self.messages.last_mut() {
            Some(m) if m.role == Role::Assistant => {
                if !m.text.is_empty() && !text.is_empty() {
                    m.text.push_str("\n\n");
                }
                m.text.push_str(text);
                truncate_chars(&mut m.text, MAX_MESSAGE_CHARS);
            }
            _ => self.push(Role::Assistant, text),
        }
    }
    pub fn selected_provider(&self) -> Option<&Provider> {
        let id = self.selected.as_deref()?;
        self.providers.iter().find(|p| p.id == id)
    }
}

/// Two notices differ only in digits (a retry counter or similar).
fn same_progress(a: &str, b: &str) -> bool {
    let strip = |s: &str| {
        s.chars()
            .filter(|c| !c.is_ascii_digit())
            .collect::<String>()
    };
    a != b && strip(a) == strip(b)
}

/// Geographic positions written in an assistant reply, as
/// "50°36′28.80″N, 001°10′00.72″W" (also with ' and " marks): (lat, lon, text).
pub fn find_positions(text: &str) -> Vec<(f64, f64, String)> {
    // One coordinate: degrees, optional minutes and seconds, hemisphere letter.
    fn coordinate(chars: &[char], mut i: usize) -> Option<(f64, char, usize)> {
        let mut parts = Vec::new();
        loop {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i == start {
                return None;
            }
            let number: f64 = chars[start..i].iter().collect::<String>().parse().ok()?;
            parts.push(number);
            let mark = *chars.get(i)?;
            if matches!(mark, '°' | '′' | '\'' | '″' | '"') {
                i += 1;
            }
            while i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
            match chars.get(i) {
                Some(h @ ('N' | 'S' | 'E' | 'W')) if parts.len() >= 2 || mark == '°' => {
                    let v = parts
                        .iter()
                        .zip([1.0, 60.0, 3600.0])
                        .map(|(n, s)| n / s)
                        .sum();
                    if parts.len() > 3 || parts[1..].iter().any(|p| *p >= 60.0) {
                        return None;
                    }
                    return Some((v, *h, i + 1));
                }
                Some(c) if c.is_ascii_digit() && parts.len() < 3 => continue,
                _ => return None,
            }
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit()
            || (i > 0 && (chars[i - 1].is_ascii_digit() || chars[i - 1] == '.'))
        {
            i += 1;
            continue;
        }
        let Some((a, ha, end)) = coordinate(&chars, i) else {
            i += 1;
            continue;
        };
        let mut j = end;
        while j < chars.len() && matches!(chars[j], ' ' | ',' | '/' | ';') {
            j += 1;
        }
        if let Some((b, hb, end2)) = coordinate(&chars, j) {
            let signed = |v: f64, h: char| if matches!(h, 'S' | 'W') { -v } else { v };
            let pair = match (ha, hb) {
                ('N' | 'S', 'E' | 'W') => Some((signed(a, ha), signed(b, hb))),
                ('E' | 'W', 'N' | 'S') => Some((signed(b, hb), signed(a, ha))),
                _ => None,
            };
            if let Some((lat, lon)) =
                pair.filter(|(lat, lon)| lat.abs() <= 90. && lon.abs() <= 180.)
            {
                out.push((lat, lon, chars[i..end2].iter().collect()));
                i = end2;
                continue;
            }
        }
        i = end;
    }
    out
}

/// Inline Markdown subset used by the clients: **bold**, `code`.
fn inline_job(
    text: &str,
    base: egui::TextFormat,
    bold: egui::TextFormat,
    code: egui::TextFormat,
    job: &mut egui::text::LayoutJob,
) {
    let mut rest = text;
    while !rest.is_empty() {
        let next_bold = rest.find("**");
        let next_code = rest.find('`');
        match (next_bold, next_code) {
            (Some(b), c) if c.is_none_or(|c| b < c) => {
                job.append(&rest[..b], 0.0, base.clone());
                let after = &rest[b + 2..];
                match after.find("**") {
                    Some(e) => {
                        job.append(&after[..e], 0.0, bold.clone());
                        rest = &after[e + 2..];
                    }
                    None => {
                        job.append(&rest[b..], 0.0, base.clone());
                        rest = "";
                    }
                }
            }
            (_, Some(c)) => {
                job.append(&rest[..c], 0.0, base.clone());
                let after = &rest[c + 1..];
                match after.find('`') {
                    Some(e) => {
                        job.append(&after[..e], 0.0, code.clone());
                        rest = &after[e + 1..];
                    }
                    None => {
                        job.append(&rest[c..], 0.0, base.clone());
                        rest = "";
                    }
                }
            }
            _ => {
                job.append(rest, 0.0, base.clone());
                rest = "";
            }
        }
    }
}

/// Block Markdown subset: headings, bullet and numbered lists, code fences.
/// The bundled faces lack U+2032/U+2033; a full-width fallback would space
/// "50°37′24″N" oddly. Display ASCII marks; copied text keeps the original.
pub fn display_marks(text: &str) -> String {
    // Emoji variation selectors have no glyph in these faces either.
    text.replace('′', "'")
        .replace('″', "\"")
        .replace(['\u{FE0F}', '\u{FE0E}'], "")
}

fn markdown(ui: &mut egui::Ui, text: &str, theme: &Theme) {
    let text = display_marks(text);
    let text = text.as_str();
    let size = egui::TextStyle::Body.resolve(ui.style()).size;
    let base = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color: theme.text,
        ..Default::default()
    };
    // The bundled bold face when this context has it; never an unbound family.
    let bold_family = egui::FontFamily::Name("ChartBold".into());
    let bold_family = if ui.fonts(|f| f.families().contains(&bold_family)) {
        bold_family
    } else {
        egui::FontFamily::Proportional
    };
    let bold = egui::TextFormat {
        font_id: egui::FontId::new(size, bold_family),
        color: theme.text,
        ..Default::default()
    };
    let code = egui::TextFormat {
        font_id: egui::FontId::monospace(size * 0.92),
        color: theme.text,
        background: theme.raised,
        ..Default::default()
    };
    let mut in_fence = false;
    let mut table: Vec<Vec<String>> = Vec::new();
    let flush_table = |ui: &mut egui::Ui, table: &mut Vec<Vec<String>>| {
        if table.is_empty() {
            return;
        }
        let rows = std::mem::take(table);
        let columns = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let spacing = 12.;
        // Share the width between columns so a cell wraps by words, not letters.
        let width =
            ((ui.available_width() - spacing * (columns - 1) as f32) / columns as f32).max(60.);
        let id = ui.next_auto_id();
        egui::Grid::new(id)
            .striped(true)
            .num_columns(columns)
            .min_col_width(width)
            .max_col_width(width)
            .spacing([spacing, 4.])
            .show(ui, |ui| {
                for (r, row) in rows.iter().enumerate() {
                    for cell in row {
                        let mut job = egui::text::LayoutJob::default();
                        job.wrap.max_width = width;
                        let format = if r == 0 { bold.clone() } else { base.clone() };
                        inline_job(cell, format, bold.clone(), code.clone(), &mut job);
                        ui.add(egui::Label::new(job).selectable(true).wrap());
                    }
                    ui.end_row();
                }
            });
    };
    for line in text.lines() {
        let trimmed = line.trim_start();
        // Markdown table rows: | a | b |. The |---| separator row is dropped.
        if !in_fence && trimmed.starts_with('|') {
            let cells: Vec<String> = trimmed
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_owned())
                .collect();
            let separator = cells
                .iter()
                .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')));
            if !separator {
                table.push(cells);
            }
            continue;
        }
        flush_table(ui, &mut table);
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = ui.available_width();
        if in_fence {
            job.append(line, 0.0, code.clone());
        } else if let Some(h) = trimmed
            .strip_prefix("### ")
            .or(trimmed.strip_prefix("## "))
            .or(trimmed.strip_prefix("# "))
        {
            inline_job(h, bold.clone(), bold.clone(), code.clone(), &mut job);
        } else if let Some(item) = trimmed.strip_prefix("- ").or(trimmed.strip_prefix("* ")) {
            let indent = (line.len() - trimmed.len()) as f32 * 4.;
            job.append("•  ", indent, base.clone());
            inline_job(item, base.clone(), bold.clone(), code.clone(), &mut job);
        } else if trimmed.is_empty() {
            ui.add_space(size * 0.4);
            continue;
        } else {
            inline_job(line, base.clone(), bold.clone(), code.clone(), &mut job);
        }
        ui.add(egui::Label::new(job).selectable(true).wrap());
    }
    flush_table(ui, &mut table);
}

/// Truncates on a character boundary and marks the cut.
pub fn truncate_chars(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(" …[truncated]");
}

/// Why the chat cannot be used right now, if it cannot.
pub fn unavailable_reason(state: &State, mcp: &crate::mcp_ui::State) -> Option<&'static str> {
    if !mcp.enabled {
        Some(
            "Enable the S-100 MCP Service to use the LLM chat. The chat uses the Codex CLI \
             or Claude Code installed on this computer.",
        )
    } else if state.status == Status::Detecting || !state.detected {
        Some("Looking for the Codex CLI and Claude Code on this computer…")
    } else if state.providers.is_empty() {
        Some(
            "No supported LLM command-line client was found. Install the Codex CLI \
             (codex) or Claude Code (claude), sign in once in a terminal, then press Refresh.",
        )
    } else {
        None
    }
}

pub fn draw(ctx: &egui::Context, state: &mut State, mcp: &crate::mcp_ui::State) {
    if !state.open {
        return;
    }
    let theme = Theme::current(ctx);
    let mut open = state.open;
    // Keep the window below the menu bar and toolbar.
    let screen = ctx.screen_rect();
    let area = egui::Rect::from_min_max(egui::pos2(screen.left(), screen.top() + 72.), screen.max);
    let shown = crate::ui_overlay_layout::centered_window("LLM Chat", ctx)
        .id(egui::Id::new("llm-chat-window"))
        .open(&mut open)
        .default_width(520.0)
        .default_height(520.0)
        .min_width(360.0)
        .max_height((area.height() - 16.).max(240.))
        .constrain_to(area)
        .resizable(true)
        .show(ctx, |ui| draw_contents(ui, state, mcp, &theme));
    state.window_rect = shown.map(|r| {
        let r = r.response.rect;
        [r.left(), r.top(), r.right(), r.bottom()]
    });
    state.open = open;
}

fn draw_contents(ui: &mut egui::Ui, state: &mut State, mcp: &crate::mcp_ui::State, theme: &Theme) {
    let unavailable = unavailable_reason(state, mcp);
    ui.horizontal(|ui| {
        let (color, label) = match &state.status {
            Status::Idle if unavailable.is_some() => (theme.muted, "Unavailable"),
            Status::Idle => (theme.success, "Ready"),
            Status::Detecting => (theme.warning, "Detecting Clients"),
            Status::Running => (theme.warning, "Running"),
            Status::Error(_) => (theme.error, "Error"),
        };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10., 10.), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4., color);
        ui.strong(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(
                    mcp.enabled && !state.running() && state.status != Status::Detecting,
                    egui::Button::new("Refresh"),
                )
                .on_hover_text("Look for installed LLM clients again")
                .clicked()
            {
                state.redetect = true;
            }
        });
    });
    if let Status::Error(e) = &state.status {
        ui.colored_label(theme.error, e);
    }
    if let Some(reason) = unavailable {
        ui.add_space(6.);
        ui.label(reason);
        return;
    }

    ui.horizontal(|ui| {
        ui.label("Client");
        let current = state
            .selected_provider()
            .map(|p| format!("{} {}", p.name, p.version))
            .unwrap_or_else(|| "Choose a Client".into());
        let mut selected = state.selected.clone();
        ui.add_enabled_ui(!state.running(), |ui| {
            egui::ComboBox::from_id_salt("llm-chat-provider")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for p in &state.providers {
                        ui.selectable_value(
                            &mut selected,
                            Some(p.id.clone()),
                            format!("{} {}", p.name, p.version),
                        )
                        .on_hover_text(&p.path);
                    }
                });
        });
        if selected != state.selected {
            state.selected = selected;
            state.new_conversation = true;
        }
        if ui
            .add_enabled(!state.running(), egui::Button::new("New Conversation"))
            .clicked()
        {
            state.new_conversation = true;
        }
    });
    ui.horizontal_wrapped(|ui| {
        let (color, text) = if state.tools_attached {
            (theme.success, "Dataset Tools Attached")
        } else {
            (theme.muted, "Dataset Tools Not Attached")
        };
        ui.colored_label(color, text);
        if !state.tools_note.is_empty() {
            ui.weak(&state.tools_note);
        }
    });
    ui.separator();

    let input_height = 64.;
    // Bounded so the window keeps its size instead of filling the screen.
    let history_height = (ui.available_height() - input_height - 48.).clamp(120., 360.);
    egui::ScrollArea::vertical()
        .id_salt("llm-chat-history")
        .max_height(history_height)
        .min_scrolled_height(history_height)
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if state.messages.is_empty() {
                ui.weak(
                    "Ask about the charts loaded in this window, S-100 products or the \
                     viewer itself. Replies come from the selected client and may be wrong; \
                     always verify against official sources before navigating.",
                );
                ui.add_space(6.);
                ui.weak("Try:");
                for example in [
                    "Which datasets are loaded?",
                    "Where is the nearest Wreck to 50°37′24.00″N, 001°09′25.21″W?",
                    "List the lights within 2 km of the chart centre.",
                ] {
                    if ui.small_button(example).clicked() {
                        state.input = example.to_owned();
                    }
                }
            }
            let mut copy = None;
            let mut show = None;
            for (index, m) in state.messages.iter().enumerate() {
                if m.role == Role::Notice {
                    // Tool use as a compact chip; other notices as one muted line.
                    let text = m
                        .text
                        .strip_prefix("Using tool ")
                        .map(|t| format!("Tool · {t}"))
                        .unwrap_or_else(|| m.text.clone());
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).small().color(theme.muted))
                            .wrap(),
                    );
                    continue;
                }
                let (who, color) = match m.role {
                    Role::User => ("You", theme.accent),
                    Role::Assistant => (
                        state
                            .selected_provider()
                            .map(|p| p.name.as_str())
                            .unwrap_or("Assistant"),
                        theme.success,
                    ),
                    Role::Notice => ("Notice", theme.muted),
                };
                egui::Frame::new()
                    .fill(if m.role == Role::User {
                        theme.raised
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .corner_radius(5)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.colored_label(color, egui::RichText::new(who).small().strong());
                            if m.role == Role::Assistant {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("Copy")
                                            .on_hover_text("Copy this reply")
                                            .clicked()
                                        {
                                            copy = Some(index);
                                        }
                                    },
                                );
                            }
                        });
                        if m.role == Role::Assistant {
                            markdown(ui, &m.text, theme);
                            let positions = find_positions(&m.text);
                            if !positions.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.weak(egui::RichText::new("Show on Chart:").small());
                                    for (lat, lon, label) in positions.into_iter().take(8) {
                                        if ui
                                            .small_button(display_marks(&label))
                                            .on_hover_text("Centre the chart here and mark it")
                                            .clicked()
                                        {
                                            show = Some([lat, lon]);
                                        }
                                    }
                                });
                            }
                        } else {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&m.text))
                                    .selectable(true)
                                    .wrap(),
                            );
                        }
                    });
            }
            if let Some(index) = copy {
                ui.ctx().copy_text(state.messages[index].text.clone());
            }
            if show.is_some() {
                state.show_position = show;
            }
            if state.running() {
                let now = ui.input(|i| i.time);
                let started = *state.run_started.get_or_insert(now);
                let tool = state
                    .messages
                    .iter()
                    .rev()
                    .take_while(|m| m.role != Role::User)
                    .find_map(|m| m.text.strip_prefix("Using tool "));
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(match tool {
                        Some(t) => format!("Working · {t} · {:.0} s", now - started),
                        None => format!("Waiting for the client · {:.0} s", now - started),
                    });
                });
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(500));
            } else {
                state.run_started = None;
                if matches!(state.status, Status::Error(_))
                    && state.messages.iter().any(|m| m.role == Role::User)
                    && ui
                        .button("Retry")
                        .on_hover_text("Send the last message again")
                        .clicked()
                {
                    state.retry = true;
                }
            }
        });
    ui.separator();

    let input_id = egui::Id::new("llm-chat-input");
    let focused = ui.ctx().memory(|m| m.has_focus(input_id));
    // Enter sends; Shift+Enter inserts a new line. Consume before the editor sees it.
    let enter = focused
        && !state.running()
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    ui.add_sized(
        [ui.available_width(), input_height],
        egui::TextEdit::multiline(&mut state.input)
            .id(input_id)
            .char_limit(MAX_INPUT_CHARS)
            .hint_text("Message (Enter to Send, Shift+Enter for a New Line)"),
    );
    ui.horizontal(|ui| {
        let can_send =
            !state.running() && state.selected.is_some() && !state.input.trim().is_empty();
        if state.running() {
            if ui.button("Stop").clicked() {
                state.stop = true;
            }
        } else if (ui
            .add_enabled(can_send, egui::Button::new("Send"))
            .clicked()
            || enter)
            && can_send
        {
            state.send = Some(std::mem::take(&mut state.input).trim().to_owned());
            ui.ctx().memory_mut(|m| m.request_focus(input_id));
        }
        ui.weak("Read-only: the client cannot change charts or files from this window.");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positions_in_replies_are_found_in_common_notations() {
        let found = find_positions(
            "가장 가까운 Wreck의 위치는 **50°36′28.80″N, 001°10′00.72″W**입니다. \
             Also 50°36.48'N 1°10.012'W and 1°10′00.72″W 50°36′28.80″N.",
        );
        assert_eq!(found.len(), 3, "{found:?}");
        for (lat, lon, _) in &found {
            assert!((lat - 50.608).abs() < 1e-3 && (lon + 1.16687).abs() < 1e-3);
        }
        assert_eq!(found[0].2, "50°36′28.80″N, 001°10′00.72″W");
        // Depths, bearings and lone latitudes are not positions.
        assert!(find_positions("Depth 7.8 m, bearing 202.2°, 50°36′N only").is_empty());
        assert!(find_positions("95°00′00″N, 001°00′00″W").is_empty());
    }
    #[test]
    fn markdown_tables_lists_and_code_render_without_panicking() {
        let ctx = egui::Context::default();
        let theme = Theme::for_profile("Day");
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                markdown(
                    ui,
                    "# Title\n| a | b |\n|---|---|\n| **1** | `2` |\n\n- item\n```\ncode\n```\nend | not a table",
                    &theme,
                );
            });
        });
    }
    #[test]
    fn display_marks_only_change_glyph_forms() {
        assert_eq!(display_marks("50°37′24″N ⚠\u{FE0F} ok"), "50°37'24\"N ⚠ ok");
    }
    #[test]
    fn counter_only_notices_replace_each_other() {
        let mut s = State::default();
        s.push(Role::Notice, "Reconnecting... 1/5 (401)");
        s.push(Role::Notice, "Reconnecting... 2/5 (401)");
        s.push(Role::Notice, "Using tool feature_nearest");
        s.push(Role::Notice, "Using tool feature_nearest");
        let texts: Vec<_> = s.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Reconnecting... 2/5 (401)",
                "Using tool feature_nearest",
                "Using tool feature_nearest"
            ]
        );
    }
    #[test]
    fn history_and_messages_are_bounded() {
        let mut s = State::default();
        for i in 0..(MAX_MESSAGES + 10) {
            s.push(Role::User, format!("{i}"));
        }
        assert_eq!(s.messages.len(), MAX_MESSAGES);
        assert_eq!(s.messages[0].text, "10");
        s.push(Role::Notice, "x");
        s.append_assistant(&"é".repeat(MAX_MESSAGE_CHARS));
        let last = &s.messages.last().unwrap().text;
        assert!(last.len() <= MAX_MESSAGE_CHARS + 20);
        assert!(last.ends_with("[truncated]"));
    }
    #[test]
    fn streamed_assistant_text_joins_one_reply() {
        let mut s = State::default();
        s.push(Role::User, "hi");
        s.append_assistant("a");
        s.append_assistant("b");
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].text, "a\n\nb");
    }
    #[test]
    fn chat_requires_service_and_a_detected_client() {
        let mut s = State::default();
        let mut mcp = crate::mcp_ui::State::default();
        assert!(unavailable_reason(&s, &mcp).unwrap().contains("Enable"));
        mcp.enabled = true;
        assert!(unavailable_reason(&s, &mcp).unwrap().contains("Looking"));
        s.detected = true;
        assert!(unavailable_reason(&s, &mcp)
            .unwrap()
            .contains("No supported"));
        s.providers.push(Provider {
            id: "codex".into(),
            ..Default::default()
        });
        assert!(unavailable_reason(&s, &mcp).is_none());
    }
    #[test]
    fn window_draws_without_blocking_in_every_state() {
        let ctx = egui::Context::default();
        let mut s = State {
            open: true,
            ..Default::default()
        };
        let mut mcp = crate::mcp_ui::State {
            enabled: true,
            ..Default::default()
        };
        for status in [Status::Detecting, Status::Idle, Status::Running] {
            s.status = status;
            s.detected = true;
            let _ = ctx.run(Default::default(), |ctx| draw(ctx, &mut s, &mcp));
            s.providers = vec![Provider {
                id: "claude".into(),
                name: "Claude Code".into(),
                version: "1".into(),
                path: "/x".into(),
            }];
            s.selected = Some("claude".into());
            s.push(Role::User, "hello");
            s.append_assistant("world");
        }
        mcp.enabled = false;
        let _ = ctx.run(Default::default(), |ctx| draw(ctx, &mut s, &mcp));
        assert!(s.send.is_none());
    }
}
