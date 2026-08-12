use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph},
};
use std::collections::HashMap;
use ulid::Ulid;

use crate::core::message::Message;
use crate::core::thread::{RootKind, resolve_anchor};

use super::app::{App, Focus};

/// Hard ceiling on visual indentation for a reply chain.
///
/// `resolve_anchor` will happily report a depth up to `MAX_THREAD_DEPTH`
/// (256), which would push content off-screen long before that. Deep chains
/// still render — they just stop indenting further and say how deep they
/// really go.
const MAX_VISUAL_DEPTH: usize = 4;

/// Spaces added per level of reply nesting.
const REPLY_INDENT_UNIT: usize = 2;

/// Why a reply is (or is not) shown as a clean attachment to its parent.
///
/// Mirrors [`RootKind`] but from the renderer's point of view: a badge is
/// chosen once per message, not once per hop, and "parent not in the loaded
/// window" is folded in as its own case rather than reported as a generic
/// resolution failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyBadge {
    /// The immediate parent is loaded and the chain above it is well formed.
    Reply,
    /// The immediate parent is not in the loaded window: scrolled out,
    /// tombstoned, or not yet synced. Indistinguishable from here, and it
    /// does not matter for rendering — either way there is nothing to quote.
    MissingParent,
    /// An ancestor anchors to itself.
    SelfReference,
    /// The chain above revisits a message it already passed.
    Cycle,
    /// The chain above is longer than `MAX_THREAD_DEPTH`.
    DepthLimit,
}

impl ReplyBadge {
    /// Classify a reply from its direct parent's availability and the full
    /// chain's [`RootKind`], read via [`resolve_anchor`].
    ///
    /// A message can have a perfectly good direct parent while an ancestor
    /// further up is the one that is broken; that still gets flagged; it is
    /// never smoothed into a plain `Reply`.
    fn classify(direct_parent_present: bool, kind: RootKind) -> Self {
        if !direct_parent_present {
            return ReplyBadge::MissingParent;
        }
        match kind {
            RootKind::SelfReference => ReplyBadge::SelfReference,
            RootKind::Cycle => ReplyBadge::Cycle,
            RootKind::DepthLimit => ReplyBadge::DepthLimit,
            RootKind::Root | RootKind::Resolved | RootKind::MissingParent => ReplyBadge::Reply,
        }
    }

    /// Badge text appended to the header, and whether it needs the alarming
    /// color (a genuinely malformed chain) or the quiet one (parent simply
    /// not on screen right now).
    fn label(self, depth: usize) -> (String, bool) {
        match self {
            ReplyBadge::Reply if depth > MAX_VISUAL_DEPTH => {
                (format!("  ↩ reply (nested {depth}x)"), false)
            }
            ReplyBadge::Reply => ("  ↩ reply".to_string(), false),
            ReplyBadge::MissingParent => ("  ↩ reply (parent not shown)".to_string(), false),
            ReplyBadge::SelfReference => ("  ↩ reply (self-reference upstream)".to_string(), true),
            ReplyBadge::Cycle => ("  ↩ reply (cycle detected)".to_string(), true),
            ReplyBadge::DepthLimit => ("  ↩ reply (depth limit reached)".to_string(), true),
        }
    }
}

/// Colors matching lazygit style
const ACTIVE_BORDER: Color = Color::Green;
const INACTIVE_TITLE: Color = Color::DarkGray;
const HELP_KEY: Color = Color::Blue;

pub fn draw(f: &mut Frame, app: &mut App) {
    // Main layout: main content | help bar at bottom
    let outer_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(1)])
        .split(f.area());

    if app.maximized() {
        // Maximized mode: no sidebar, messages fill the entire area
        // Set layout areas to zero-size rects so mouse detection skips sidebar
        app.set_layout_areas(Rect::default(), outer_chunks[0]);
        app.set_agents_area(Rect::default());

        draw_messages(f, app, outer_chunks[0]);
        draw_status(f, app, outer_chunks[1]);
    } else {
        // Normal mode: sidebar | messages
        // Sidebar contains: channels on top, agents below
        // Use dynamic sidebar width from app state (user can resize by dragging border)
        let sidebar_width = app.sidebar_width();
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(sidebar_width), // sidebar (channels + agents) - user-resizable
                Constraint::Min(40),               // messages - expands with available space
            ])
            .split(outer_chunks[0]);

        // Split sidebar vertically: channels on top, agents below
        // Use user-override height if set (from drag resize), otherwise auto-calculate
        let agent_height = if let Some(h) = app.agents_height_override() {
            h
        } else {
            let agent_count = app.agent_statuses().len();
            ((agent_count * 2) + 2).clamp(5, 20) as u16
        };

        let sidebar_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),               // channels (flexible, min 5 lines)
                Constraint::Length(agent_height), // agents (user-resizable or dynamic)
            ])
            .split(main_chunks[0]);

        // Update cached layout areas for mouse detection
        app.set_layout_areas(sidebar_chunks[0], main_chunks[1]);
        app.set_agents_area(sidebar_chunks[1]);

        draw_channels(f, app, sidebar_chunks[0]);
        draw_agents(f, app, sidebar_chunks[1]);
        draw_messages(f, app, main_chunks[1]);
        draw_status(f, app, outer_chunks[1]);
    }

    // Draw help overlay if active
    if app.show_help() {
        draw_help_overlay(f);
    }
}

fn draw_channels(f: &mut Frame, app: &mut App, area: Rect) {
    let selected = app.selected_channel_index();
    let public_count = app.channels().len();
    let is_focused = app.focus() == Focus::Channels;

    // Calculate viewport height (account for top and bottom borders)
    let viewport_height = area.height.saturating_sub(2) as usize;

    // Update scroll to keep selected channel visible
    app.update_channels_scroll(viewport_height);
    let channels_scroll = app.channels_scroll();

    let mut all_items: Vec<(usize, ListItem)> = Vec::new();

    // Public channels
    for (i, ch) in app.channels().iter().enumerate() {
        let new_count = app.new_message_count(ch);
        let is_selected = i == selected;

        let line = format_channel_line(&format!("#{}", ch), new_count, is_selected, is_focused);
        all_items.push((i, ListItem::new(line)));
    }

    // DM section separator (if there are DMs)
    if !app.dm_channels().is_empty() {
        let separator_idx = public_count;
        all_items.push((
            separator_idx,
            ListItem::new(Line::from(vec![Span::styled(
                "── DMs ──",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )])),
        ));
    }

    // DM channels - show as Agent1↔Agent2
    for (i, dm) in app.dm_channels().iter().enumerate() {
        let global_idx = public_count + if !app.dm_channels().is_empty() { 1 } else { 0 } + i;
        let display_name = format_dm_channel(dm);
        let new_count = app.new_message_count(dm);
        let is_selected = global_idx == selected;

        let line = format_channel_line_dm(&display_name, new_count, is_selected, is_focused);
        all_items.push((global_idx, ListItem::new(line)));
    }

    // Apply scroll: only show items within viewport range
    let visible_items: Vec<ListItem> = all_items
        .into_iter()
        .skip(channels_scroll)
        .take(viewport_height)
        .map(|(_, item)| item)
        .collect();

    let (border_style, title_style) = if is_focused {
        (
            Style::default().fg(ACTIVE_BORDER),
            Style::default().fg(ACTIVE_BORDER),
        )
    } else {
        (Style::default(), Style::default().fg(INACTIVE_TITLE))
    };

    let list = List::new(visible_items).block(
        Block::default()
            .title(Span::styled(" Conversations ", title_style))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style),
    );

    f.render_widget(list, area);
}

/// Format a DM channel name as "Agent1↔Agent2"
fn format_dm_channel(channel: &str) -> String {
    if let Some(agents) = channel.strip_prefix("_dm_") {
        let parts: Vec<&str> = agents.splitn(2, '_').collect();
        if parts.len() == 2 {
            return format!("{}↔{}", parts[0], parts[1]);
        }
    }
    channel.to_string()
}

/// Format a channel line with optional new message count
fn format_channel_line(
    name: &str,
    new_count: usize,
    is_selected: bool,
    is_focused: bool,
) -> Line<'static> {
    let name_style = if is_selected && is_focused {
        Style::default()
            .fg(ACTIVE_BORDER)
            .add_modifier(Modifier::BOLD)
    } else if is_selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let mut spans = vec![Span::styled(name.to_string(), name_style)];

    if new_count > 0 {
        spans.push(Span::styled(
            format!(" {}", new_count),
            Style::default().fg(Color::Yellow),
        ));
    }

    Line::from(spans)
}

/// Format a DM channel line with optional new message count
fn format_channel_line_dm(
    name: &str,
    new_count: usize,
    is_selected: bool,
    is_focused: bool,
) -> Line<'static> {
    let name_style = if is_selected && is_focused {
        Style::default()
            .fg(ACTIVE_BORDER)
            .add_modifier(Modifier::BOLD)
    } else if is_selected {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Magenta)
    };

    let mut spans = vec![Span::styled(name.to_string(), name_style)];

    if new_count > 0 {
        spans.push(Span::styled(
            format!(" {}", new_count),
            Style::default().fg(Color::Yellow),
        ));
    }

    Line::from(spans)
}

fn draw_agents(f: &mut Frame, app: &App, area: Rect) {
    use super::app::{AgentInfo, AgentStatus};
    use std::collections::BTreeMap;

    let agents = app.agent_statuses();

    // Build a trie from agent names: each "/" creates a nesting level.
    // e.g., "chief-dev/0/agent0" → chief-dev → 0 → agent0
    struct Node<'a> {
        info: Option<&'a AgentInfo>,
        children: BTreeMap<String, Node<'a>>,
    }

    impl<'a> Node<'a> {
        fn new() -> Self {
            Self {
                info: None,
                children: BTreeMap::new(),
            }
        }

        fn flatten(&self, depth: usize, result: &mut Vec<(usize, String, Option<&'a AgentInfo>)>) {
            for (name, child) in &self.children {
                result.push((depth, name.clone(), child.info));
                child.flatten(depth + 1, result);
            }
        }
    }

    let mut tree = Node::new();
    for agent_info in agents {
        let segments: Vec<&str> = agent_info.name.split('/').collect();
        let mut current = &mut tree;
        for seg in segments {
            current = current
                .children
                .entry(seg.to_string())
                .or_insert_with(Node::new);
        }
        current.info = Some(agent_info);
    }

    let mut render_list = Vec::new();
    tree.flatten(0, &mut render_list);

    // Render each node as list items
    let items: Vec<ListItem> = render_list
        .iter()
        .flat_map(|(depth, name, info)| {
            let indent = "  ".repeat(*depth);

            let (indicator, name_style) = if let Some(info) = info {
                match info.status {
                    AgentStatus::Online => (
                        Span::styled("● ", Style::default().fg(Color::Green)),
                        Style::default().fg(Color::White),
                    ),
                    AgentStatus::Afk | AgentStatus::Offline => (
                        Span::styled("● ", Style::default().fg(Color::DarkGray)),
                        Style::default().fg(Color::DarkGray),
                    ),
                }
            } else {
                // Synthetic parent node (no agent:// claim) — offline glyph
                (
                    Span::styled("● ", Style::default().fg(Color::DarkGray)),
                    Style::default().fg(Color::DarkGray),
                )
            };

            let name_line = Line::from(vec![
                Span::raw(indent.clone()),
                indicator,
                Span::styled(name.to_string(), name_style),
            ]);

            if let Some(info) = info
                && info.status == AgentStatus::Online
                && let Some(msg) = &info.message
            {
                let msg_indent = format!("{indent}  ");
                let truncated = if msg.len() > 32 {
                    format!("{}...", &msg[..29])
                } else {
                    msg.clone()
                };
                let msg_line = Line::from(vec![
                    Span::raw(msg_indent),
                    Span::styled(
                        truncated,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]);
                vec![ListItem::new(name_line), ListItem::new(msg_line)]
            } else {
                vec![ListItem::new(name_line)]
            }
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                " Agents ",
                Style::default().fg(INACTIVE_TITLE),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default()),
    );

    f.render_widget(list, area);
}

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    // Calculate input height based on content (min 1 line, max 10 lines)
    let num_lines = app.input.lines().len().clamp(1, 10);
    let input_height = (num_lines as u16) + 2; // +2 for borders

    // Split area: messages on top, input bar at bottom
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),               // Messages need at least 5 lines
            Constraint::Length(input_height), // Input bar: dynamic based on content
        ])
        .split(area);

    let messages_area = chunks[0];
    let input_area = chunks[1];

    // Update cached input area for mouse detection
    app.set_input_area(input_area);

    let raw_channel_name = app
        .current_channel()
        .unwrap_or_else(|| "general".to_string());

    // Format channel names nicely
    let channel_name = if raw_channel_name.starts_with("_dm_") {
        format_dm_channel(&raw_channel_name)
    } else {
        format!("#{}", raw_channel_name)
    };

    let is_focused = app.focus() == Focus::Messages;
    let (border_style, title_style) = if is_focused {
        (
            Style::default().fg(ACTIVE_BORDER),
            Style::default().fg(ACTIVE_BORDER),
        )
    } else {
        (Style::default(), Style::default().fg(INACTIVE_TITLE))
    };

    // Calculate dimensions first
    let inner_width = messages_area.width.saturating_sub(2) as usize; // Account for borders
    let inner_height = messages_area.height.saturating_sub(2) as usize;

    // Convert all messages to lines, inserting separator if needed
    let messages = app.messages();
    // Reply resolution only ever looks inside the currently loaded window: a
    // parent scrolled out of it is exactly as unavailable to the renderer as
    // one that was tombstoned or never synced. See `ReplyBadge`.
    let by_id: HashMap<Ulid, &Message> = messages.iter().map(|m| (m.id, m)).collect();
    let mut lines: Vec<Line> = Vec::new();

    let separator_pos = if app.has_separator(&raw_channel_name) {
        app.separator_position(&raw_channel_name)
    } else {
        None
    };

    let hide_system = app.hide_system_messages();

    for (idx, msg) in messages.iter().enumerate() {
        if hide_system && is_system_message(msg) {
            continue;
        }

        // Insert separator BEFORE the first new message
        if let Some(pos) = separator_pos
            && idx == pos
        {
            lines.push(create_separator_line(inner_width));
        }

        lines.extend(format_message(msg, inner_width, &by_id));
    }

    // Total lines is just the count since we pre-wrapped in format_message
    let total_lines = lines.len();

    // Clamp scroll to valid range
    let max_scroll = total_lines.saturating_sub(inner_height);
    app.clamp_scroll_lines(max_scroll, inner_height);

    let scroll = app.message_scroll();

    // Use Paragraph's scroll feature - scroll from bottom
    // scroll=0 means show bottom, scroll=max means show top
    let scroll_from_top = max_scroll.saturating_sub(scroll);

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(Span::styled(format!(" {} ", channel_name), title_style))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(border_style),
        )
        // Wrapping is done manually in format_message to maintain 4-space indent
        .scroll((scroll_from_top as u16, 0));

    f.render_widget(paragraph, messages_area);

    // Draw input bar
    draw_input_bar(f, app, input_area);
}

fn draw_input_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.focus() == Focus::Input;

    let (border_style, title_style) = if is_focused {
        (
            Style::default().fg(ACTIVE_BORDER),
            Style::default().fg(ACTIVE_BORDER),
        )
    } else {
        (Style::default(), Style::default().fg(INACTIVE_TITLE))
    };

    // Clone textarea and set block styling
    let mut textarea = app.input.clone();

    // Remove cursor line highlighting
    textarea.set_cursor_line_style(Style::default());

    // Hide cursor when not focused
    if !is_focused {
        textarea.set_cursor_style(Style::default());
    }

    textarea.set_block(
        Block::default()
            .title(Span::styled(" chat - ctrl+s to send ", title_style))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style),
    );

    f.render_widget(&textarea, area);
}

fn create_separator_line(width: usize) -> Line<'static> {
    let text = " New Messages ";
    let text_len = text.len();

    // Calculate padding (leave space for borders)
    let available_width = width.saturating_sub(2);

    if text_len >= available_width {
        // Not enough space, just show the text
        return Line::from(Span::styled(
            text,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Create centered separator with box drawing characters
    let line_char = "─"; // U+2500
    let total_line_len = available_width.saturating_sub(text_len);
    let left_len = total_line_len / 2;
    let right_len = total_line_len - left_len;

    let line_style = Style::default().fg(Color::DarkGray);
    let text_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD);

    Line::from(vec![
        Span::styled(line_char.repeat(left_len), line_style),
        Span::styled(text, text_style),
        Span::styled(line_char.repeat(right_len), line_style),
    ])
}

fn format_message(
    msg: &Message,
    max_width: usize,
    by_id: &HashMap<Ulid, &Message>,
) -> Vec<Line<'static>> {
    let local_time: DateTime<Local> = msg.ts.with_timezone(&Local);
    let datetime_str = local_time.format("%Y-%m-%d %H:%M").to_string();

    // Early return for deleted message tombstones (should be filtered out, but defensive)
    if matches!(
        &msg.meta,
        Some(crate::core::message::MessageMeta::Deleted { .. })
    ) {
        let dim_italic = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        return vec![
            Line::from(vec![
                Span::styled("\u{F140C} ", dim_italic),
                Span::styled(format!("[{}] ", datetime_str), dim_italic),
                Span::styled("[message deleted]", dim_italic),
            ]),
            Line::from(""),
        ];
    }

    if is_system_message(msg) {
        return format_system_message(msg, &datetime_str, max_width);
    }

    let agent_color = agent_color(&msg.agent);

    // Every message written before threading existed — and every message
    // that simply is not a reply — has no `parent_id`. That is the untouched
    // flat path: no connector, no indent, no badge, byte-for-byte what this
    // function produced before replies existed.
    let reply = msg.parent_id().map(|parent_id| {
        let anchor = resolve_anchor(msg.id, by_id);
        let direct_parent = by_id.get(&parent_id).copied();
        let depth = anchor.depth.clamp(1, MAX_VISUAL_DEPTH);
        let badge = ReplyBadge::classify(direct_parent.is_some(), anchor.kind);
        (direct_parent, depth, anchor.depth, badge)
    });

    let (connector_indent, body_indent_extra) = match &reply {
        Some((_, depth, _, _)) => (
            REPLY_INDENT_UNIT * depth.saturating_sub(1),
            REPLY_INDENT_UNIT * depth,
        ),
        None => (0, 0),
    };

    let mut result_lines = Vec::new();

    // First line: ● agent [timestamp] — or, for a reply, an indented
    // connector standing in for the bullet.
    let mut header_spans = if reply.is_some() {
        vec![
            Span::raw(" ".repeat(connector_indent)),
            Span::styled("└─ ", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![Span::styled("● ", Style::default().fg(Color::DarkGray))]
    };
    header_spans.push(Span::styled(
        msg.agent.clone(),
        Style::default()
            .fg(agent_color)
            .add_modifier(Modifier::BOLD),
    ));
    header_spans.push(Span::styled(
        format!(" [{}]", datetime_str),
        Style::default().fg(Color::DarkGray),
    ));

    // Add labels after timestamp if present
    if !msg.labels.is_empty() {
        for label in &msg.labels {
            header_spans.push(Span::styled(
                format!(" [{}]", label),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    // Add attachment indicator after labels if present
    if !msg.attachments.is_empty() {
        // Check if any attachments are missing
        let missing_count = msg.attachments.iter().filter(|a| !a.is_available()).count();

        if missing_count > 0 {
            // Some attachments are missing - show with warning color
            header_spans.push(Span::styled(
                format!(" [{}⚠]", msg.attachments.len()),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            // All attachments available
            header_spans.push(Span::styled(
                format!(" [{}]", msg.attachments.len()),
                Style::default().fg(Color::Magenta),
            ));
        }
    }

    // Explicit reply affordance: never let a reply look like a top-level
    // message, and never claim a clean attachment when the chain above it is
    // missing, cyclic, self-referencing, or over the depth cap.
    if let Some((_, _, raw_depth, badge)) = &reply {
        let (text, alarming) = badge.label(*raw_depth);
        let color = if alarming { Color::Red } else { Color::Cyan };
        header_spans.push(Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::ITALIC),
        ));
    }

    result_lines.push(Line::from(header_spans));

    // One-line preview of the parent, so a reply is still legible when the
    // parent has scrolled out of view. Skipped only when there is no parent
    // to quote (`ReplyBadge::MissingParent`) — the badge above already says
    // so.
    if let Some((Some(parent), _, _, _)) = &reply {
        let preview_indent = " ".repeat(2 + body_indent_extra);
        result_lines.push(Line::from(vec![
            Span::raw(preview_indent),
            Span::styled(
                format!("↳ {}: ", parent.agent),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::styled(
                preview_text(&parent.body, 60),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    // Process message body - wrap and add @mention highlighting
    let body_lines: Vec<&str> = msg.body.lines().collect();
    let body_prefix = " ".repeat(2 + body_indent_extra);

    for body_line in body_lines {
        // Wrap the line, leaving room for the indent + 2-space right padding
        let wrapped = wrap_text(body_line, max_width.saturating_sub(4 + body_indent_extra));

        for wrapped_line in wrapped {
            // Highlight @mentions in blue
            let mut line_spans = vec![Span::raw(body_prefix.clone())];
            line_spans.extend(highlight_mentions(&wrapped_line));
            result_lines.push(Line::from(line_spans));
        }
    }

    // Add blank line after message
    result_lines.push(Line::from(""));

    result_lines
}

/// First line of a message body, trimmed and truncated to `max_chars` — just
/// enough to recognize the parent by when quoting it inline.
fn preview_text(body: &str, max_chars: usize) -> String {
    let first_line = body.lines().next().unwrap_or("").trim();
    let char_count = first_line.chars().count();

    if char_count <= max_chars {
        first_line.to_string()
    } else {
        let truncated: String = first_line
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{truncated}…")
    }
}

/// Whether ctrl+h hides this message.
///
/// Delegates to `Message::is_system` so the TUI and `rite history` cannot
/// drift apart about what a channel contains.
///
/// This used to also cover claim, release, and claim-extension records. It no
/// longer does: 34,209 of the 34,313 such records live in `#claims`, so with
/// hiding now on by default that channel would open empty.
fn is_system_message(msg: &crate::core::message::Message) -> bool {
    msg.is_system()
}

/// Format a system message with lightning bolt glyph and dim italic styling.
fn format_system_message(
    msg: &crate::core::message::Message,
    datetime_str: &str,
    max_width: usize,
) -> Vec<Line<'static>> {
    let dim_italic = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    let mut result_lines = Vec::new();

    // Header: 󱐌 system [timestamp]
    let header_spans = vec![
        Span::styled("\u{F140C} ", dim_italic),
        Span::styled(msg.agent.clone(), dim_italic),
        Span::styled(format!(" [{}]", datetime_str), dim_italic),
    ];
    result_lines.push(Line::from(header_spans));

    // Body lines in dim italic
    let body_lines: Vec<&str> = msg.body.lines().collect();
    for body_line in body_lines {
        let wrapped = wrap_text(body_line, max_width.saturating_sub(4));
        for wrapped_line in wrapped {
            result_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(wrapped_line, dim_italic),
            ]));
        }
    }

    // Blank line after message
    result_lines.push(Line::from(""));

    result_lines
}

/// Highlight @mentions in text by coloring them blue
fn highlight_mentions(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut last_end = 0;

    // Highlight @mentions (blue bold) and !flags (yellow)
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'@' {
            // Found potential mention start
            let start = i;
            i += 1;

            // Collect alphanumeric, hyphens, underscores, and slashes
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                i += 1;
            }

            // Only treat as mention if we found at least one char after @
            if i > start + 1 {
                if start > last_end {
                    spans.push(Span::raw(text[last_end..start].to_string()));
                }
                spans.push(Span::styled(
                    text[start..i].to_string(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ));
                last_end = i;
            }
        } else if bytes[i] == b'!' && (i == 0 || bytes[i - 1] == b' ') {
            // Found potential !flag at start of text or after a space
            let start = i;
            i += 1;

            // Collect alphanumeric, hyphens, underscores
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
            {
                i += 1;
            }

            // Only treat as flag if we found at least one char after !
            // and the token ends at whitespace or end of string
            if i > start + 1 && (i == bytes.len() || bytes[i] == b' ') {
                if start > last_end {
                    spans.push(Span::raw(text[last_end..start].to_string()));
                }
                spans.push(Span::styled(
                    text[start..i].to_string(),
                    Style::default().fg(Color::Yellow),
                ));
                last_end = i;
            }
        } else {
            i += 1;
        }
    }

    // Add remaining text
    if last_end < text.len() {
        spans.push(Span::raw(text[last_end..].to_string()));
    }

    // If no highlights found, return single span
    if spans.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }

    spans
}

/// Wrap text to fit within max_width, breaking on word boundaries when possible.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let mut result = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for word in text.split_whitespace() {
        let word_len = word.len();

        // If this word alone exceeds max_width, force-break it
        if word_len > max_width {
            // Flush current line if any
            if !current_line.is_empty() {
                result.push(current_line.trim_end().to_string());
                current_line.clear();
                current_width = 0;
            }

            // Break the long word into chunks
            for chunk in word.chars().collect::<Vec<_>>().chunks(max_width) {
                result.push(chunk.iter().collect());
            }
            continue;
        }

        // Check if adding this word (plus space) exceeds width
        let space_needed = if current_width == 0 { 0 } else { 1 }; // Space before word
        if current_width + space_needed + word_len > max_width {
            // Flush current line and start new one
            if !current_line.is_empty() {
                result.push(current_line.trim_end().to_string());
            }
            current_line = word.to_string();
            current_width = word_len;
        } else {
            // Add word to current line
            if current_width > 0 {
                current_line.push(' ');
                current_width += 1;
            }
            current_line.push_str(word);
            current_width += word_len;
        }
    }

    // Flush remaining line
    if !current_line.is_empty() {
        result.push(current_line.trim_end().to_string());
    }

    // Handle empty text
    if result.is_empty() {
        result.push(String::new());
    }

    result
}

fn agent_color(name: &str) -> Color {
    let hash: usize = name.bytes().map(|b| b as usize).sum();
    let colors = [
        Color::Cyan,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
    ];
    colors[hash % colors.len()]
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    // Show different key hints based on focus
    let status = if app.focus() == Focus::Input {
        Line::from(vec![
            Span::styled(" [ctrl+s] ", Style::default().fg(HELP_KEY)),
            Span::raw("send  "),
            Span::styled("[enter] ", Style::default().fg(HELP_KEY)),
            Span::raw("newline  "),
            Span::styled("[ctrl+h] ", Style::default().fg(HELP_KEY)),
            Span::raw(if app.hide_system_messages() {
                "show sys  "
            } else {
                "hide sys  "
            }),
            Span::styled("[ctrl+f] ", Style::default().fg(HELP_KEY)),
            Span::raw("maximize  "),
            Span::styled("[esc] ", Style::default().fg(HELP_KEY)),
            Span::raw("clear  "),
            Span::styled("[ctrl+q] ", Style::default().fg(HELP_KEY)),
            Span::raw("quit"),
        ])
    } else {
        Line::from(vec![
            Span::styled(" [Tab] ", Style::default().fg(HELP_KEY)),
            Span::raw("pane  "),
            Span::styled("[j/k] ", Style::default().fg(HELP_KEY)),
            Span::raw("scroll  "),
            Span::styled("[u/d] ", Style::default().fg(HELP_KEY)),
            Span::raw("½page  "),
            Span::styled("[ctrl+h] ", Style::default().fg(HELP_KEY)),
            Span::raw(if app.hide_system_messages() {
                "show sys  "
            } else {
                "hide sys  "
            }),
            Span::styled("[ctrl+f] ", Style::default().fg(HELP_KEY)),
            Span::raw("maximize  "),
            Span::styled("[?] ", Style::default().fg(HELP_KEY)),
            Span::raw("help  "),
            Span::styled("[q] ", Style::default().fg(HELP_KEY)),
            Span::raw("quit"),
        ])
    };

    let paragraph = Paragraph::new(status);
    f.render_widget(paragraph, area);
}

fn draw_help_overlay(f: &mut Frame) {
    let area = f.area();

    // Center a box in the middle of the screen
    let width = 60.min(area.width.saturating_sub(4));
    let height = 25.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    // Clear the area behind the popup
    f.render_widget(Clear, popup_area);

    let help_text = vec![
        Line::from(vec![Span::styled(
            "Navigation",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Tab       ", Style::default().fg(HELP_KEY)),
            Span::raw("Switch between panes"),
        ]),
        Line::from(vec![
            Span::styled("  h/l       ", Style::default().fg(HELP_KEY)),
            Span::raw("Switch to left/right pane"),
        ]),
        Line::from(vec![
            Span::styled("  j/k       ", Style::default().fg(HELP_KEY)),
            Span::raw("Scroll down/up (1 line)"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl-j/k  ", Style::default().fg(HELP_KEY)),
            Span::raw("Scroll down/up (10 lines)"),
        ]),
        Line::from(vec![
            Span::styled("  u/d       ", Style::default().fg(HELP_KEY)),
            Span::raw("Scroll up/down (half page)"),
        ]),
        Line::from(vec![
            Span::styled("  b/f       ", Style::default().fg(HELP_KEY)),
            Span::raw("Scroll up/down (full page)"),
        ]),
        Line::from(vec![
            Span::styled("  g/G       ", Style::default().fg(HELP_KEY)),
            Span::raw("Jump to top/bottom"),
        ]),
        Line::from(vec![
            Span::styled("  Enter     ", Style::default().fg(HELP_KEY)),
            Span::raw("Select channel"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl-F    ", Style::default().fg(HELP_KEY)),
            Span::raw("Toggle maximize channel view"),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Input",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Tab       ", Style::default().fg(HELP_KEY)),
            Span::raw("Focus input bar"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl-S    ", Style::default().fg(HELP_KEY)),
            Span::raw("Send message (when input focused)"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl-Q    ", Style::default().fg(HELP_KEY)),
            Span::raw("Quit (works from input)"),
        ]),
        Line::from(vec![
            Span::styled("  Enter     ", Style::default().fg(HELP_KEY)),
            Span::raw("Newline (when input focused)"),
        ]),
        Line::from(vec![
            Span::styled("  Esc       ", Style::default().fg(HELP_KEY)),
            Span::raw("Clear input (when input focused)"),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Mouse",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Click     ", Style::default().fg(HELP_KEY)),
            Span::raw("Focus pane / select channel"),
        ]),
        Line::from(vec![
            Span::styled("  Wheel     ", Style::default().fg(HELP_KEY)),
            Span::raw("Scroll messages"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let help = Paragraph::new(help_text)
        .block(
            Block::default()
                .title(Span::styled(
                    " Keybindings ",
                    Style::default()
                        .fg(ACTIVE_BORDER)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACTIVE_BORDER)),
        )
        .alignment(Alignment::Left);

    f.render_widget(help, popup_area);
}

#[cfg(test)]
mod tests {
    use super::is_system_message;
    use crate::core::message::{Message, MessageMeta};

    /// Claim bookkeeping is **not** hidden, which reverses what this test
    /// asserted before hiding became the default.
    ///
    /// It used to be folded in with system messages, which was harmless while
    /// ctrl+h defaulted to showing everything. Now that both the TUI and
    /// `rite history` hide by default, folding it in would open `#claims` —
    /// 34,209 of the 34,313 claim records live there — on an empty screen.
    #[test]
    fn does_not_treat_claim_extensions_as_system_messages() {
        let msg = Message::new("botbox-dev", "claims", "Claim extended").with_meta(
            MessageMeta::ClaimExtended {
                patterns: vec!["agent://botbox-dev".to_string()],
                ttl_secs: 600,
            },
        );

        assert!(!is_system_message(&msg));
    }

    /// A hook firing is what the filter exists for.
    #[test]
    fn treats_hook_firings_as_system_messages() {
        use crate::core::message::SystemEvent;

        let msg = Message::new("system", "rite", "Hook hk-abc fired: vessel spawn").with_meta(
            MessageMeta::System {
                event: SystemEvent::HookFired {
                    hook_id: "hk-abc".to_string(),
                    command: vec!["vessel".to_string(), "spawn".to_string()],
                },
            },
        );

        assert!(is_system_message(&msg));
    }

    #[test]
    fn does_not_treat_regular_messages_as_system_messages() {
        let msg = Message::new("botbox-dev", "general", "hello");

        assert!(!is_system_message(&msg));
    }

    mod reply_rendering {
        use super::super::{MAX_VISUAL_DEPTH, format_message};
        use crate::core::message::Message;
        use std::collections::HashMap;
        use ulid::Ulid;

        /// Flatten a rendered line's spans into plain text for assertions.
        fn line_text(line: &ratatui::text::Line) -> String {
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        }

        fn render(msg: &Message, by_id: &HashMap<Ulid, &Message>) -> Vec<String> {
            format_message(msg, 100, by_id)
                .iter()
                .map(line_text)
                .collect()
        }

        /// The untouched default path: a message with no `reply_to` renders
        /// exactly as it did before threading existed — plain bullet, no
        /// connector, no badge, no preview line.
        #[test]
        fn flat_message_has_no_reply_decoration() {
            let msg = Message::new("agent", "general", "hello");
            let by_id: HashMap<Ulid, &Message> = HashMap::new();

            let lines = render(&msg, &by_id);
            assert!(lines[0].starts_with("● "), "{:?}", lines);
            assert!(!lines[0].contains("↩"), "{:?}", lines);
            assert!(!lines.iter().any(|l| l.contains("↳")), "{:?}", lines);
        }

        /// A reply whose parent is loaded gets a connector, an explicit
        /// "reply" affordance, and a one-line quote of the parent — the quote
        /// is what keeps the reply legible once the parent scrolls away.
        #[test]
        fn reply_with_loaded_parent_shows_connector_and_preview() {
            let parent = Message::new("asker", "general", "who owns review 42?");
            let reply = Message::new("answerer", "general", "I do").with_reply_to(parent.id);
            let by_id: HashMap<Ulid, &Message> = [(parent.id, &parent), (reply.id, &reply)]
                .into_iter()
                .collect();

            let lines = render(&reply, &by_id);
            assert!(lines[0].contains("└─"), "{:?}", lines);
            assert!(lines[0].contains("↩ reply"), "{:?}", lines);
            assert!(!lines[0].contains("not shown"), "{:?}", lines);
            assert!(
                lines[1].contains("asker") && lines[1].contains("who owns review 42?"),
                "{:?}",
                lines
            );
        }

        /// The parent is not in the loaded window (scrolled off, tombstoned,
        /// or simply unsynced — indistinguishable from here). The reply must
        /// still say it is a reply, but it must not fabricate a preview, and
        /// it must never render as if it were a plain top-level message.
        #[test]
        fn reply_with_missing_parent_is_flagged_not_smoothed_over() {
            let orphan_parent = Ulid::new();
            let reply = Message::new("agent", "general", "on it").with_reply_to(orphan_parent);
            let by_id: HashMap<Ulid, &Message> = [(reply.id, &reply)].into_iter().collect();

            let lines = render(&reply, &by_id);
            assert!(lines[0].contains("└─"), "{:?}", lines);
            assert!(lines[0].contains("parent not shown"), "{:?}", lines);
            // No preview line: nothing to quote. The body must follow
            // directly after the header.
            assert!(!lines[1].contains("↳"), "{:?}", lines);
        }

        /// A tombstoned parent is removed from the loaded set exactly like
        /// one that never synced — same degrade path, same badge.
        #[test]
        fn reply_to_a_tombstoned_parent_degrades_like_a_missing_one() {
            let tombstoned_id = Ulid::new();
            let reply = Message::new("agent", "general", "thanks").with_reply_to(tombstoned_id);
            // The tombstoned message and its tombstone record are both
            // filtered out by `read_messages` before the TUI ever sees them,
            // so the loaded window simply does not contain `tombstoned_id`.
            let by_id: HashMap<Ulid, &Message> = [(reply.id, &reply)].into_iter().collect();

            let lines = render(&reply, &by_id);
            assert!(lines[0].contains("parent not shown"), "{:?}", lines);
        }

        /// Deep chains stop indenting at `MAX_VISUAL_DEPTH` rather than
        /// pushing content off-screen, but the badge still says how deep the
        /// chain really goes.
        #[test]
        fn deep_chain_caps_visual_indent_but_reports_true_depth() {
            let mut messages = vec![Message::new("agent", "general", "root")];
            for i in 0..(MAX_VISUAL_DEPTH + 3) {
                let parent = messages.last().unwrap().clone();
                messages.push(
                    Message::new("agent", "general", format!("hop {i}")).with_reply_to(parent.id),
                );
            }
            let by_id: HashMap<Ulid, &Message> = messages.iter().map(|m| (m.id, m)).collect();

            let at_cap = &messages[MAX_VISUAL_DEPTH];
            let past_cap = messages.last().unwrap();

            let at_cap_lines = render(at_cap, &by_id);
            let past_cap_lines = render(past_cap, &by_id);

            // Indentation (everything before the connector glyph) stops
            // growing once the cap is reached.
            let indent_of = |line: &str| line.find('└').unwrap_or(0);
            assert_eq!(
                indent_of(&at_cap_lines[0]),
                indent_of(&past_cap_lines[0]),
                "indent must not grow past the cap: {:?} vs {:?}",
                at_cap_lines[0],
                past_cap_lines[0]
            );

            // But the badge on the deeper message still names its real depth.
            assert!(!at_cap_lines[0].contains("nested"), "{:?}", at_cap_lines);
            assert!(past_cap_lines[0].contains("nested"), "{:?}", past_cap_lines);
        }

        /// An ancestor that anchors to itself is surfaced, not smoothed into
        /// a plain reply badge.
        #[test]
        fn self_reference_upstream_is_surfaced() {
            let mut looped = Message::new("agent", "general", "me");
            looped.reply_to = Some(looped.id);
            let child = Message::new("agent", "general", "child").with_reply_to(looped.id);
            let by_id: HashMap<Ulid, &Message> = [(looped.id, &looped), (child.id, &child)]
                .into_iter()
                .collect();

            let lines = render(&child, &by_id);
            assert!(lines[0].contains("self-reference"), "{:?}", lines);
        }

        /// Two messages anchored to each other form a cycle; the walk
        /// terminates and the reply badge says so instead of pretending the
        /// chain is clean.
        #[test]
        fn cycle_upstream_is_surfaced() {
            let mut a = Message::new("agent", "general", "a");
            let mut b = Message::new("agent", "general", "b");
            a.reply_to = Some(b.id);
            b.reply_to = Some(a.id);
            let by_id: HashMap<Ulid, &Message> = [(a.id, &a), (b.id, &b)].into_iter().collect();

            let lines = render(&a, &by_id);
            assert!(lines[0].contains("cycle detected"), "{:?}", lines);
        }
    }
}
