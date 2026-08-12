use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use ratatui::prelude::*;
use tui_textarea::TextArea;

use std::path::Path;
use std::time::Duration;

use crate::core::claim::FileClaim;
use crate::core::identity::resolve_agent;
use crate::core::message::{Message, read_last_n_messages, read_messages_from_offset};
use crate::core::project::{channel_path, channels_dir, claims_path, state_path, statuses_path};
use crate::core::status::AgentStatusEntry;
use crate::storage::jsonl::read_records;
use crate::storage::state::ProjectState;
use crate::storage::watch::{debounce_events, filter_channel_events, watch_directory};

use super::ui;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Online,  // Active claim, plenty of time remaining
    Afk,     // Active claim, close to expiration (< 5 min)
    Offline, // No active claim
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub status: AgentStatus,
    pub message: Option<String>,
}

pub struct App {
    current_agent: Option<String>,
    channels: Vec<String>,
    dm_channels: Vec<String>,
    selected_channel: usize,
    messages: Vec<Message>,
    message_scroll: usize,
    /// Scroll offset for the channels list to keep selected channel visible
    channels_scroll: usize,
    should_quit: bool,
    focus: Focus,
    channel_offset: u64,
    show_help: bool,
    /// Cached viewport height for page scroll calculations
    viewport_height: usize,
    /// Cached layout areas for mouse click detection
    channels_area: Rect,
    messages_area: Rect,
    /// File sizes when TUI started (for new message indicators)
    initial_sizes: std::collections::HashMap<String, u64>,
    /// Current new message counts per channel
    new_message_counts: std::collections::HashMap<String, usize>,
    /// Previous new message counts (for detecting changes to trigger notifications)
    previous_message_counts: std::collections::HashMap<String, usize>,
    /// Timestamp when current channel was focused (for auto-clear timer)
    channel_focused_at: Option<std::time::Instant>,
    /// Which channels have visible separators
    separator_visible: std::collections::HashSet<String>,
    /// Input textarea for composing messages
    pub input: TextArea<'static>,
    /// Cached input area for mouse click detection
    input_area: Rect,
    /// Agent status information (for agent pane)
    agent_statuses: Vec<AgentInfo>,
    /// Last time agent statuses were updated (for rate limiting)
    last_agent_update: Option<std::time::Instant>,
    /// Sidebar width (columns) - user can resize
    sidebar_width: u16,
    /// Whether user is currently dragging the sidebar border
    dragging_border: bool,
    /// Cached agents pane area for mouse detection
    agents_area: Rect,
    /// Whether user is currently dragging the sidebar split border (channels/agents)
    dragging_sidebar_split: bool,
    /// User-override for agents pane height (None = auto-calculate)
    agents_height: Option<u16>,
    /// Whether the channel/chat view is maximized (sidebar hidden)
    maximized: bool,
    /// Whether system messages are hidden in the current channel
    hide_system_messages: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Focus {
    Channels,
    Messages,
    Input,
}

impl App {
    pub fn new(initial_channel: Option<String>) -> Result<Self> {
        // Get agent from env var (no explicit flag for TUI)
        let current_agent = resolve_agent(None);

        let (channels, dm_channels) = list_channels()?;

        // Find initial channel index (search in both channels and DMs)
        let selected_channel = if let Some(ch) = &initial_channel {
            channels
                .iter()
                .position(|c| c == ch)
                .or_else(|| {
                    dm_channels
                        .iter()
                        .position(|c| c == ch)
                        .map(|i| i + channels.len())
                })
                .unwrap_or(0)
        } else {
            channels.iter().position(|c| c == "general").unwrap_or(0)
        };

        // Capture initial file sizes for new message indicators
        let initial_sizes = capture_channel_sizes(&channels, &dm_channels);

        let mut app = Self {
            current_agent,
            channels,
            dm_channels,
            selected_channel,
            messages: Vec::new(),
            message_scroll: 0,
            channels_scroll: 0,
            should_quit: false,
            focus: Focus::Channels,
            channel_offset: 0,
            show_help: false,
            viewport_height: 20,
            channels_area: Rect::default(),
            messages_area: Rect::default(),
            initial_sizes,
            new_message_counts: std::collections::HashMap::new(),
            previous_message_counts: std::collections::HashMap::new(),
            channel_focused_at: None,
            separator_visible: std::collections::HashSet::new(),
            input: TextArea::default(),
            input_area: Rect::default(),
            agent_statuses: Vec::new(),
            last_agent_update: None,
            sidebar_width: 30, // Default width: 30 columns
            dragging_border: false,
            agents_area: Rect::default(),
            dragging_sidebar_split: false,
            agents_height: None,
            maximized: false,
            // Hidden by default, matching `rite history`. On a busy channel
            // this is a fifth to nearly half of every screen, and it is never
            // what someone opened the channel to read. ctrl+h brings it back,
            // and the footer says so.
            hide_system_messages: true,
        };

        app.update_new_message_counts();
        app.update_agent_statuses();
        app.load_messages()?;

        Ok(app)
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        // Enable mouse capture
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;

        // Set up file watcher
        let channels_path = channels_dir();
        let (_watcher, rx) = watch_directory(&channels_path)?;

        let result = self.run_loop(terminal, &rx);

        // Disable mouse capture on exit
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);

        result
    }

    fn run_loop<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        rx: &std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    ) -> Result<()> {
        loop {
            terminal.draw(|f| ui::draw(f, self))?;

            // Handle input first for responsiveness
            if event::poll(Duration::from_millis(16))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key.code, key.modifiers)?;
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse)?;
                    }
                    _ => {}
                }
            }

            if self.should_quit {
                break;
            }

            // Check for file changes (non-blocking, short timeout)
            let changes = debounce_events(rx, Duration::from_millis(1));
            let channel_changes = filter_channel_events(changes);

            // Check if any changed channels are new (not in our current list)
            let has_new_channels = channel_changes
                .iter()
                .any(|ch| !self.channels.contains(ch) && !self.dm_channels.contains(ch));

            if has_new_channels {
                // Refresh channel list to pick up new channels
                self.refresh_channels()?;
            }

            // Update unread counts and agent statuses when any channel changes
            if !channel_changes.is_empty() {
                self.update_new_message_counts();
                self.update_agent_statuses();
            }

            if let Some(current) = self.current_channel() {
                if channel_changes.contains(&current) {
                    self.refresh_messages()?;
                }

                // Check if separator should auto-clear (after 2 seconds of viewing)
                if let Some(focused_at) = self.channel_focused_at
                    && focused_at.elapsed() >= Duration::from_secs(2)
                {
                    self.clear_separator_and_mark_read(&current);
                }
            }
        }

        Ok(())
    }

    fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> Result<()> {
        // Dismiss help overlay on any key
        if self.show_help {
            self.show_help = false;
            return Ok(());
        }

        let ctrl = modifiers.contains(KeyModifiers::CONTROL);

        // Global keys (with ctrl modifier check)
        if let KeyCode::Char('q') = key
            && (ctrl || self.focus != Focus::Input)
        {
            // Ctrl+Q quits from anywhere, plain 'q' quits when not in input
            self.should_quit = true;
            return Ok(());
        }

        // Ctrl+F toggles maximize from any focus
        if let KeyCode::Char('f') = key
            && ctrl
        {
            self.maximized = !self.maximized;
            // When maximizing from sidebar, switch focus to messages
            if self.maximized && self.focus == Focus::Channels {
                self.focus = Focus::Messages;
            }
            return Ok(());
        }

        // Ctrl+H toggles system message visibility
        if let KeyCode::Char('h') = key
            && ctrl
        {
            self.hide_system_messages = !self.hide_system_messages;
            return Ok(());
        }

        match key {
            KeyCode::Esc => {
                // Esc clears input if focused, otherwise quits
                if self.focus == Focus::Input {
                    self.input = TextArea::default();
                } else {
                    self.should_quit = true;
                }
                return Ok(());
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Channels => Focus::Messages,
                    Focus::Messages => Focus::Input,
                    // Skip Channels pane when maximized (sidebar hidden)
                    Focus::Input if self.maximized => Focus::Messages,
                    Focus::Input => Focus::Channels,
                };
                return Ok(());
            }
            KeyCode::Char('h') if self.focus != Focus::Input => {
                self.focus = Focus::Channels;
                return Ok(());
            }
            KeyCode::Char('l') if self.focus != Focus::Input => {
                self.focus = Focus::Messages;
                return Ok(());
            }
            KeyCode::Char('?') if self.focus != Focus::Input => {
                self.show_help = true;
                return Ok(());
            }
            _ => {}
        }

        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let half_page = self.viewport_height / 2;
        let full_page = self.viewport_height;

        // Focus-specific keys
        match self.focus {
            Focus::Messages => match key {
                // Single line scroll
                KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                    self.message_scroll = self.message_scroll.saturating_add(1);
                }
                KeyCode::Down | KeyCode::Char('j') if !ctrl => {
                    self.message_scroll = self.message_scroll.saturating_sub(1);
                }
                // 10 line scroll with Ctrl
                KeyCode::Char('k') if ctrl => {
                    self.message_scroll = self.message_scroll.saturating_add(10);
                }
                KeyCode::Char('j') if ctrl => {
                    self.message_scroll = self.message_scroll.saturating_sub(10);
                }
                // Half page scroll (vim u/d)
                KeyCode::Char('u') => {
                    self.message_scroll = self.message_scroll.saturating_add(half_page);
                }
                KeyCode::Char('d') => {
                    self.message_scroll = self.message_scroll.saturating_sub(half_page);
                }
                // Full page scroll (less b/f)
                KeyCode::Char('b') | KeyCode::PageUp => {
                    self.message_scroll = self.message_scroll.saturating_add(full_page);
                }
                KeyCode::Char('f') | KeyCode::PageDown => {
                    self.message_scroll = self.message_scroll.saturating_sub(full_page);
                }
                // Jump to top/bottom
                KeyCode::Home | KeyCode::Char('g') => {
                    self.message_scroll = usize::MAX / 2;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.message_scroll = 0;
                }
                _ => {}
            },
            Focus::Channels => match key {
                KeyCode::Up | KeyCode::Char('k') if self.selected_channel > 0 => {
                    self.selected_channel -= 1;
                    self.load_messages()?;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let total = self.total_channel_count();
                    if self.selected_channel < total.saturating_sub(1) {
                        self.selected_channel += 1;
                        self.load_messages()?;
                    }
                }
                KeyCode::Enter => {
                    self.focus = Focus::Messages;
                }
                _ => {}
            },
            Focus::Input => {
                let ctrl = modifiers.contains(KeyModifiers::CONTROL);

                match key {
                    // Ctrl+S sends the message
                    KeyCode::Char('s') if ctrl => {
                        self.send_input_message()?;
                    }
                    // Ctrl+J inserts newline (intercept before tui-textarea's delete-to-beginning binding)
                    KeyCode::Char('j') if ctrl => {
                        self.input.insert_str("\n");
                    }
                    // Forward all other keys to tui-textarea (including Enter for newlines)
                    _ => {
                        use tui_textarea::Input;
                        self.input.input(Input {
                            key: key.into(),
                            ctrl: modifiers.contains(KeyModifiers::CONTROL),
                            alt: modifiers.contains(KeyModifiers::ALT),
                            shift: modifiers.contains(KeyModifiers::SHIFT),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> Result<()> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let x = mouse.column;
                let y = mouse.row;

                // Check if click is on the vertical border between sidebar and messages
                // Border is at x = sidebar_width (the right edge of sidebar)
                if x == self.sidebar_width && y < self.messages_area.y + self.messages_area.height {
                    self.dragging_border = true;
                    return Ok(());
                }

                // Check if click is on the horizontal border between channels and agents
                if x < self.sidebar_width && self.agents_area.height > 0 && y == self.agents_area.y
                {
                    self.dragging_sidebar_split = true;
                    return Ok(());
                }

                // Check if click is in channels area
                if x >= self.channels_area.x
                    && x < self.channels_area.x + self.channels_area.width
                    && y >= self.channels_area.y
                    && y < self.channels_area.y + self.channels_area.height
                {
                    self.focus = Focus::Channels;

                    // Calculate which channel was clicked (accounting for border and scroll offset)
                    let clicked_row = (y - self.channels_area.y).saturating_sub(1) as usize
                        + self.channels_scroll;
                    let total = self.total_channel_count();

                    // Account for DM separator if present
                    let dm_separator_offset =
                        if !self.dm_channels.is_empty() && clicked_row >= self.channels.len() {
                            1 // Skip the "-- DMs --" separator
                        } else {
                            0
                        };

                    let adjusted_row = if clicked_row > self.channels.len() {
                        clicked_row.saturating_sub(dm_separator_offset)
                    } else {
                        clicked_row
                    };

                    if adjusted_row < total {
                        self.selected_channel = adjusted_row;
                        self.load_messages()?;
                    }
                }
                // Check input area BEFORE messages area - input is a subset of the right panel,
                // so we must check it first or messages_area will match input clicks
                else if x >= self.input_area.x
                    && x < self.input_area.x + self.input_area.width
                    && y >= self.input_area.y
                    && y < self.input_area.y + self.input_area.height
                {
                    self.focus = Focus::Input;
                }
                // Check if click is in messages area
                else if x >= self.messages_area.x
                    && x < self.messages_area.x + self.messages_area.width
                    && y >= self.messages_area.y
                    && y < self.messages_area.y + self.messages_area.height
                {
                    self.focus = Focus::Messages;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Stop dragging on mouse up
                self.dragging_border = false;
                self.dragging_sidebar_split = false;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging_border {
                    let x = mouse.column;
                    // Update sidebar width, enforcing minimum widths for both panes
                    // Min sidebar: 20 columns, Min messages: 40 columns
                    let terminal_width = self.messages_area.x + self.messages_area.width;
                    let min_sidebar = 10;
                    let min_messages = 40;
                    let max_sidebar = terminal_width.saturating_sub(min_messages);

                    self.sidebar_width = x.clamp(min_sidebar, max_sidebar);
                } else if self.dragging_sidebar_split {
                    let y = mouse.row;
                    let sidebar_top = self.channels_area.y;
                    let sidebar_bottom = self.agents_area.y + self.agents_area.height;
                    let total = sidebar_bottom.saturating_sub(sidebar_top);
                    let min_channels: u16 = 5;
                    let min_agents: u16 = 3;

                    let channels_height = y
                        .saturating_sub(sidebar_top)
                        .clamp(min_channels, total.saturating_sub(min_agents));
                    self.agents_height = Some(total.saturating_sub(channels_height));
                }
            }
            MouseEventKind::ScrollUp if self.focus == Focus::Messages => {
                self.message_scroll = self.message_scroll.saturating_add(3);
            }
            MouseEventKind::ScrollDown if self.focus == Focus::Messages => {
                self.message_scroll = self.message_scroll.saturating_sub(3);
            }
            _ => {}
        }
        Ok(())
    }

    fn load_messages(&mut self) -> Result<()> {
        if let Some(channel) = self.current_channel() {
            let path = channel_path(&channel);
            self.messages = read_last_n_messages(&path, 100)?;
            self.channel_offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            self.message_scroll = 0;

            // Start timer for separator auto-clear when viewing channel with new messages
            if self.new_message_count(&channel) > 0 {
                self.channel_focused_at = Some(std::time::Instant::now());
                self.separator_visible.insert(channel.clone());
            } else {
                self.channel_focused_at = None;
                self.separator_visible.remove(&channel);
            }
        }
        Ok(())
    }

    fn refresh_messages(&mut self) -> Result<()> {
        if let Some(channel) = self.current_channel() {
            let path = channel_path(&channel);
            let (new_msgs, new_offset): (Vec<Message>, u64) =
                read_messages_from_offset(&path, self.channel_offset)?;

            self.messages.extend(new_msgs);
            self.channel_offset = new_offset;

            // Keep only last 100
            if self.messages.len() > 100 {
                let drain_count = self.messages.len() - 100;
                self.messages.drain(0..drain_count);
            }
        }

        // Update new message counts for all channels
        self.update_new_message_counts();

        // If current channel now has new messages, show separator
        if let Some(channel) = self.current_channel()
            && self.new_message_count(&channel) > 0
            && !self.separator_visible.contains(&channel)
        {
            self.channel_focused_at = Some(std::time::Instant::now());
            self.separator_visible.insert(channel);
        }

        Ok(())
    }

    fn refresh_channels(&mut self) -> Result<()> {
        let (channels, dm_channels) = list_channels()?;

        // Preserve the current selection if possible
        let current_name = self.current_channel();

        self.channels = channels;
        self.dm_channels = dm_channels;

        // Try to maintain selection on the same channel
        if let Some(name) = current_name {
            if let Some(idx) = self.channels.iter().position(|c| c == &name) {
                self.selected_channel = idx;
            } else if let Some(idx) = self.dm_channels.iter().position(|c| c == &name) {
                self.selected_channel = self.channels.len() + idx;
            } else {
                // Channel disappeared, reset to first
                self.selected_channel = 0;
            }
        }

        // Update initial sizes for new channels
        for channel in self.channels.iter().chain(self.dm_channels.iter()) {
            if !self.initial_sizes.contains_key(channel) {
                let path = channel_path(channel);
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.initial_sizes.insert(channel.clone(), size);
            }
        }

        Ok(())
    }

    fn clear_separator_and_mark_read(&mut self, channel: &str) {
        // Mark all messages as read by updating initial_sizes
        let path = channel_path(channel);
        let current_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.initial_sizes.insert(channel.to_string(), current_size);

        // Clear the separator and counters
        self.separator_visible.remove(channel);
        self.new_message_counts.remove(channel);
        self.previous_message_counts.remove(channel);
        self.channel_focused_at = None;
    }

    fn update_new_message_counts(&mut self) {
        let all_channels: Vec<String> = self
            .channels
            .iter()
            .chain(self.dm_channels.iter())
            .cloned()
            .collect();

        let current_channel = self.current_channel();

        for channel in all_channels {
            let path = channel_path(&channel);
            let current_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let initial_size = self.initial_sizes.get(&channel).copied().unwrap_or(0);

            let previous_count = self
                .previous_message_counts
                .get(&channel)
                .copied()
                .unwrap_or(0);

            if current_size > initial_size {
                // Count newlines (messages) in the new portion
                let count = count_new_messages(&path, initial_size, current_size);
                if count > 0 {
                    self.new_message_counts.insert(channel.clone(), count);

                    // Send notification if this is a background channel with new messages
                    if Some(&channel) != current_channel.as_ref()
                        && count > previous_count
                        && let Err(e) = self.send_notification(&channel, count - previous_count)
                    {
                        eprintln!("Failed to send notification for #{}: {}", channel, e);
                    }
                }
            } else {
                self.new_message_counts.remove(&channel);
            }
        }

        // Update previous counts for next iteration
        self.previous_message_counts = self.new_message_counts.clone();
    }

    fn send_notification(&self, channel: &str, new_count: usize) -> Result<()> {
        use notify_rust::Notification;

        // Read the latest message to show in notification (best effort)
        let path = channel_path(channel);
        let messages: Vec<Message> = read_last_n_messages(&path, 1).unwrap_or_default();

        let (summary, body) = if let Some(msg) = messages.last() {
            let summary = format!("#{}", channel);
            let body = if msg.body.len() > 100 {
                format!("{}: {}...", msg.agent, &msg.body[..97])
            } else {
                format!("{}: {}", msg.agent, msg.body)
            };
            (summary, body)
        } else {
            let summary = format!("#{}", channel);
            let body = format!(
                "{} new message{}",
                new_count,
                if new_count == 1 { "" } else { "s" }
            );
            (summary, body)
        };

        // Try to send notification, ignore errors (notifications are best-effort)
        match Notification::new()
            .summary(&summary)
            .body(&body)
            .timeout(10000)
            .show()
        {
            Ok(_) => Ok(()),
            Err(e) => {
                // Log but don't fail - notifications might not be available
                eprintln!(
                    "Notification failed (this is normal on some systems): {}",
                    e
                );
                Ok(())
            }
        }
    }

    fn update_agent_statuses(&mut self) {
        use chrono::Utc;

        // Rate limit: only update every 2 seconds
        if let Some(last_update) = self.last_agent_update
            && last_update.elapsed() < Duration::from_secs(2)
        {
            return;
        }
        self.last_agent_update = Some(std::time::Instant::now());

        let now = Utc::now();
        let mut agent_map: std::collections::HashMap<String, (AgentStatus, Option<String>)> =
            std::collections::HashMap::new();

        // 1) Read claims for presence (online/afk)
        //    An agent is Online if they hold ANY active claim, not just agent:// claims.
        if let Ok(all_claims) = read_records::<FileClaim>(&claims_path()) {
            // Build latest state per claim ID
            let mut active: std::collections::HashMap<ulid::Ulid, FileClaim> =
                std::collections::HashMap::new();
            for claim in all_claims {
                active.insert(claim.id, claim);
            }

            // Sort by timestamp for deterministic processing
            let mut sorted: Vec<_> = active.values().collect();
            sorted.sort_by_key(|c| c.ts);

            // First pass: extract agent names from agent:// patterns (for display name mapping)
            for claim in &sorted {
                for pattern in &claim.patterns {
                    if let Some(agent_name) = pattern.strip_prefix("agent://") {
                        let time_remaining = (claim.expires_at - now).num_seconds();
                        let time_since_update = (now - claim.ts).num_seconds();

                        let is_subagent = agent_name.contains('/');
                        let status = if claim.active && time_remaining >= 0 {
                            AgentStatus::Online
                        } else if !is_subagent && time_since_update < 86400 {
                            AgentStatus::Afk
                        } else {
                            continue;
                        };
                        agent_map.insert(agent_name.to_string(), (status, None));
                    }
                }
            }

            // Second pass: check ALL claims by agent field — any valid claim means Online
            for claim in &sorted {
                let is_subagent = claim.agent.contains('/');
                if claim.is_valid() {
                    // Agent holds an active, non-expired claim — they're Online
                    agent_map
                        .entry(claim.agent.clone())
                        .and_modify(|(s, _)| *s = AgentStatus::Online)
                        .or_insert((AgentStatus::Online, None));
                } else if !is_subagent {
                    let time_since_update = (now - claim.ts).num_seconds();
                    if time_since_update < 86400 {
                        // Recently active but no valid claims — Afk (don't downgrade Online)
                        agent_map
                            .entry(claim.agent.clone())
                            .or_insert((AgentStatus::Afk, None));
                    }
                }
            }
        }

        // 2) Read statuses.jsonl for status messages (overlay onto claims)
        if let Ok(all_entries) = read_records::<AgentStatusEntry>(&statuses_path()) {
            // Build latest status per agent
            let mut latest: std::collections::HashMap<String, AgentStatusEntry> =
                std::collections::HashMap::new();
            for entry in all_entries {
                latest.insert(entry.agent.clone(), entry);
            }

            for entry in latest.into_values() {
                let msg = if entry.message.is_empty() {
                    None
                } else {
                    Some(entry.message.clone())
                };

                if entry.is_valid() {
                    // Active status: ensure agent is shown as Online with message
                    agent_map
                        .entry(entry.agent)
                        .and_modify(|(_, m)| *m = msg.clone())
                        .or_insert((AgentStatus::Online, msg));
                } else if entry.is_recently_expired() {
                    // Recently expired status: add message if agent already in map
                    agent_map.entry(entry.agent).and_modify(|(_, m)| {
                        if m.is_none() {
                            *m = msg.clone();
                        }
                    });
                }
            }
        }

        // Convert to sorted vector
        let mut statuses: Vec<AgentInfo> = agent_map
            .into_iter()
            .map(|(name, (status, message))| AgentInfo {
                name,
                status,
                message,
            })
            .collect();

        // Sort: Online first, then Afk; alphabetically within each group
        statuses.sort_by(|a, b| {
            use std::cmp::Ordering;
            let status_order = |s: &AgentStatus| match s {
                AgentStatus::Online => 0,
                AgentStatus::Afk => 1,
                AgentStatus::Offline => 2,
            };
            match status_order(&a.status).cmp(&status_order(&b.status)) {
                Ordering::Equal => a.name.cmp(&b.name),
                other => other,
            }
        });

        self.agent_statuses = statuses;
    }

    pub fn agent_statuses(&self) -> &[AgentInfo] {
        &self.agent_statuses
    }

    pub fn new_message_count(&self, channel: &str) -> usize {
        self.new_message_counts.get(channel).copied().unwrap_or(0)
    }

    pub fn has_separator(&self, channel: &str) -> bool {
        self.separator_visible.contains(channel)
    }

    pub fn separator_position(&self, channel: &str) -> Option<usize> {
        // Calculate how many messages are "old" (before initial_sizes)
        let initial_size = self.initial_sizes.get(channel).copied()?;
        if initial_size == 0 {
            return None; // All messages are new, no separator needed
        }

        // Count messages that were present when TUI started
        let path = channel_path(channel);
        let current_size = std::fs::metadata(&path).map(|m| m.len()).ok()?;

        if current_size <= initial_size {
            return None; // No new messages
        }

        // Count new messages
        let new_count = count_new_messages(&path, initial_size, current_size);
        if new_count == 0 || new_count >= self.messages.len() {
            return None; // All messages are new or counting failed
        }

        // Messages Vec is oldest-first chronologically
        // Separator should appear BEFORE the first new message
        // If we have 5 messages and 2 are new, separator goes at index 3
        // (after old messages at 0,1,2 and before new messages at 3,4)
        let old_count = self.messages.len().saturating_sub(new_count);
        if old_count == 0 {
            return None; // All messages are new
        }

        Some(old_count)
    }

    pub fn current_channel(&self) -> Option<String> {
        let total_public = self.channels.len();
        if self.selected_channel < total_public {
            self.channels.get(self.selected_channel).cloned()
        } else {
            self.dm_channels
                .get(self.selected_channel - total_public)
                .cloned()
        }
    }

    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    pub fn dm_channels(&self) -> &[String] {
        &self.dm_channels
    }

    pub fn selected_channel_index(&self) -> usize {
        self.selected_channel
    }

    /// Total number of channels (public + DMs)
    pub fn total_channel_count(&self) -> usize {
        self.channels.len() + self.dm_channels.len()
    }

    fn send_input_message(&mut self) -> Result<()> {
        use crate::core::flags::parse_flags;
        use crate::core::identity::require_agent;
        use crate::core::message::Message;
        use crate::core::project::channel_path;
        use crate::storage::jsonl::append_record;

        // Get message text from input (join lines and trim)
        let text = self.input.lines().join("\n").trim().to_string();

        // Don't send empty messages
        if text.is_empty() {
            return Ok(());
        }

        // Get current channel
        let channel = match self.current_channel() {
            Some(ch) => ch,
            None => return Ok(()), // No channel selected
        };

        // Parse !flags from message body
        let parsed = parse_flags(&text);
        let hook_flags = parsed.flags;

        // Get user from $USER environment variable
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        let agent_name = require_agent(Some(&user))?;

        // Create and send message directly (without CLI output)
        // Store original body — flags are meaningful to downstream consumers
        let msg = Message::new(&agent_name, &channel, &text).with_labels(vec!["human".to_string()]);
        let path = channel_path(&channel);
        append_record(&path, &msg)?;

        // Evaluate channel hooks in a background thread so OnExit hooks
        // don't freeze the TUI event loop.
        // Skip if !nohooks flag was in the message
        if !hook_flags.suppress_all() {
            let ch = channel.clone();
            let mid = msg.id.to_string();
            let body = text.clone();
            let meta = msg.meta.clone();
            let agent = agent_name.clone();
            let mentions = msg.mentions.clone();
            std::thread::spawn(move || {
                crate::cli::hooks::evaluate_hooks_with_flags(
                    &ch,
                    &mid,
                    &body,
                    meta.as_ref(),
                    &agent,
                    &mentions,
                    &hook_flags,
                );
            });
        }

        // Clear input after sending
        self.input = TextArea::default();

        // Refresh messages to show the new message
        self.refresh_messages()?;

        Ok(())
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn current_agent(&self) -> Option<&str> {
        self.current_agent.as_deref()
    }

    pub fn message_scroll(&self) -> usize {
        self.message_scroll
    }

    pub fn channels_scroll(&self) -> usize {
        self.channels_scroll
    }

    /// Clamp scroll offset to valid range based on rendered line count.
    /// Called by UI after layout and line wrapping is calculated.
    pub fn clamp_scroll_lines(&mut self, max_scroll: usize, viewport_height: usize) {
        self.viewport_height = viewport_height;
        self.message_scroll = self.message_scroll.min(max_scroll);
    }

    /// Update channels scroll to keep the selected channel visible.
    /// The selected channel should be kept within the visible viewport.
    pub fn update_channels_scroll(&mut self, viewport_height: usize) {
        let selected = self.selected_channel;

        // Calculate total items to show: public channels + separator (if any DMs) + DMs
        let _total_items = self.channels.len()
            + if !self.dm_channels.is_empty() { 1 } else { 0 }
            + self.dm_channels.len();

        // Ensure the selected channel is visible by scrolling if necessary
        if selected < self.channels_scroll {
            // Selected channel is above the current scroll position, scroll up to show it
            self.channels_scroll = selected;
        } else if selected >= self.channels_scroll + viewport_height {
            // Selected channel is below the visible area, scroll down to show it at the bottom
            self.channels_scroll = selected.saturating_sub(viewport_height.saturating_sub(1));
        }
    }

    pub fn show_help(&self) -> bool {
        self.show_help
    }

    /// Update cached layout areas for mouse click detection
    pub fn set_layout_areas(&mut self, channels: Rect, messages: Rect) {
        self.channels_area = channels;
        self.messages_area = messages;
    }

    /// Update cached input area for mouse click detection
    pub fn set_input_area(&mut self, input: Rect) {
        self.input_area = input;
    }

    /// Get current sidebar width (0 when maximized)
    pub fn sidebar_width(&self) -> u16 {
        if self.maximized {
            0
        } else {
            self.sidebar_width
        }
    }

    /// Whether the channel/chat view is currently maximized
    pub fn maximized(&self) -> bool {
        self.maximized
    }

    /// Update cached agents pane area for mouse detection
    pub fn set_agents_area(&mut self, area: Rect) {
        self.agents_area = area;
    }

    /// Get user-override for agents pane height (None = auto-calculate)
    pub fn agents_height_override(&self) -> Option<u16> {
        self.agents_height
    }

    /// Whether system messages are currently hidden
    pub fn hide_system_messages(&self) -> bool {
        self.hide_system_messages
    }
}

fn list_channels() -> Result<(Vec<String>, Vec<String>)> {
    let channels = channels_dir();
    let mut public_channels: Vec<(String, std::time::SystemTime)> = Vec::new();
    let mut dm_channels: Vec<(String, std::time::SystemTime)> = Vec::new();

    // Load closed channels list from state
    let state_file = ProjectState::new(state_path());
    let state = state_file.load()?;
    let closed_channels = &state.closed_channels;

    if channels.exists() {
        for entry in std::fs::read_dir(&channels)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
            {
                // Skip closed channels
                if closed_channels.contains(&name.to_string()) {
                    continue;
                }

                let modified = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

                if name.starts_with("_dm_") {
                    // Show ALL DMs - omniscient observer view
                    dm_channels.push((name.to_string(), modified));
                } else {
                    public_channels.push((name.to_string(), modified));
                }
            }
        }
    }

    // Sort alphabetically
    let mut public: Vec<String> = public_channels.into_iter().map(|(n, _)| n).collect();
    let mut dms: Vec<String> = dm_channels.into_iter().map(|(n, _)| n).collect();

    public.sort();
    dms.sort();

    // Keep #general at the top of public channels
    if let Some(pos) = public.iter().position(|c| c == "general") {
        let general = public.remove(pos);
        public.insert(0, general);
    }

    Ok((public, dms))
}

/// Capture initial file sizes for all channels
fn capture_channel_sizes(
    channels: &[String],
    dm_channels: &[String],
) -> std::collections::HashMap<String, u64> {
    let mut sizes = std::collections::HashMap::new();

    for channel in channels.iter().chain(dm_channels.iter()) {
        let path = channel_path(channel);
        if let Ok(meta) = std::fs::metadata(&path) {
            sizes.insert(channel.clone(), meta.len());
        }
    }

    sizes
}

/// Count new messages (lines) between two file offsets
fn count_new_messages(path: &Path, start: u64, end: u64) -> usize {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };

    if file.seek(SeekFrom::Start(start)).is_err() {
        return 0;
    }

    let bytes_to_read = (end - start) as usize;
    let mut buffer = vec![0u8; bytes_to_read.min(1024 * 1024)]; // Cap at 1MB

    let Ok(bytes_read) = file.read(&mut buffer) else {
        return 0;
    };

    // Count newlines
    buffer[..bytes_read].iter().filter(|&&b| b == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serial_test::serial;
    use std::env;
    use std::fs;
    use tempfile::TempDir;

    const DATA_DIR_ENV_VAR: &str = "RITE_DATA_DIR";

    #[test]
    #[serial]
    fn test_ctrl_h_toggles_system_visibility() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp_path);
            env::set_var("RITE_AGENT", "test-agent");
        }

        let mut app = App::new(None).unwrap();
        // Hidden by default, matching `rite history`.
        assert!(app.hide_system_messages());

        // Toggle off — bring the bookkeeping back
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL)
            .unwrap();
        assert!(!app.hide_system_messages());

        // Toggle on again
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL)
            .unwrap();
        assert!(app.hide_system_messages());

        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
            env::remove_var("RITE_AGENT");
        }
    }

    #[test]
    #[serial]
    fn test_refresh_channels_picks_up_new_channels() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp_path);
            env::set_var("RITE_AGENT", "test-agent");
        }

        // Create channels directory
        let channels_dir = temp.path().join("channels");
        fs::create_dir_all(&channels_dir).unwrap();

        // Create initial channel
        fs::write(
            channels_dir.join("general.jsonl"),
            r#"{"ts":"2024-01-01T00:00:00Z","id":"01HX0000000000000000000000","agent":"test","channel":"general","body":"hello"}"#,
        )
        .unwrap();

        // Create app - should see only general
        let mut app = App::new(None).unwrap();
        assert_eq!(app.channels.len(), 1);
        assert_eq!(app.channels[0], "general");

        // Create a new channel while app is "running"
        fs::write(
            channels_dir.join("new-channel.jsonl"),
            r#"{"ts":"2024-01-01T00:01:00Z","id":"01HX0000000000000000000001","agent":"test","channel":"new-channel","body":"hi"}"#,
        )
        .unwrap();

        // Refresh channels - should now see both
        app.refresh_channels().unwrap();
        assert_eq!(app.channels.len(), 2);
        assert!(app.channels.contains(&"general".to_string()));
        assert!(app.channels.contains(&"new-channel".to_string()));

        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
            env::remove_var("RITE_AGENT");
        }
    }

    #[test]
    #[serial]
    fn test_refresh_channels_preserves_selection() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_str().unwrap();

        unsafe {
            env::set_var(DATA_DIR_ENV_VAR, temp_path);
            env::set_var("RITE_AGENT", "test-agent");
        }

        let channels_dir = temp.path().join("channels");
        fs::create_dir_all(&channels_dir).unwrap();

        // Create two channels
        fs::write(channels_dir.join("general.jsonl"), "").unwrap();
        fs::write(channels_dir.join("backend.jsonl"), "").unwrap();

        let mut app = App::new(Some("backend".to_string())).unwrap();
        assert_eq!(app.selected_channel, 1); // backend is second after general

        // Create a new channel
        fs::write(channels_dir.join("frontend.jsonl"), "").unwrap();

        // Refresh - should still have backend selected
        let before_name = app.current_channel().unwrap();
        app.refresh_channels().unwrap();
        let after_name = app.current_channel().unwrap();

        assert_eq!(before_name, after_name);
        assert_eq!(after_name, "backend");

        unsafe {
            env::remove_var(DATA_DIR_ENV_VAR);
            env::remove_var("RITE_AGENT");
        }
    }
}
