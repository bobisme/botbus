use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};

use crate::attachments::{AttachmentCache, AttachmentSource, attachments_dir};
use crate::cli::send;
use crate::core::message::{
    Attachment, AttachmentContent, Message, MessageMeta, read_messages_from_offset,
};
use crate::core::project::{channel_path, channels_dir, state_path};
use crate::storage::state::ProjectState;
use crate::telegram::client::{TelegramClient, TelegramMessage, Update};
use crate::telegram::config::{TelegramConfig, TelegramConfigStore};

const POLL_TIMEOUT_SECS: u64 = 30;
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
const WATCH_INTERVAL: Duration = Duration::from_millis(500);
const TELEGRAM_MAX_CHARS: usize = 4000;
const TELEGRAM_CAPTION_MAX_CHARS: usize = 1024;
const SYSTEM_CHANNEL_PREFIX: char = '_';
/// Maximum incoming message size (10KB) to prevent memory exhaustion
const MAX_INCOMING_MESSAGE_LEN: usize = 10 * 1024;

pub async fn run(config: TelegramConfig, store: TelegramConfigStore) -> Result<()> {
    let client = TelegramClient::new(&config.bot_token)?;
    let config = Arc::new(Mutex::new(config));

    // Shutdown signal - sender held here, receivers given to tasks
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Install Ctrl+C handler
    let shutdown_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_signal.send(true);
        }
    });

    // Initial topic sync
    {
        let mut guard = config.lock().await;
        if sync_topics(&client, &mut guard).await? {
            store.save(&guard)?;
        }
    }

    // Spawn poll task
    let poll_handle = {
        let client = client.clone();
        let config = Arc::clone(&config);
        let store = store.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move { poll_loop(client, config, store, shutdown_rx).await })
    };

    // Spawn watch task
    let watch_handle = {
        let client = client.clone();
        let config = Arc::clone(&config);
        let store = store.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move { watch_loop(client, config, store, shutdown_rx).await })
    };

    // Wait for either task to complete (or fail)
    tokio::select! {
        result = poll_handle => {
            let _ = shutdown_tx.send(true);
            result??;
        }
        result = watch_handle => {
            let _ = shutdown_tx.send(true);
            result??;
        }
    }

    Ok(())
}

async fn poll_loop(
    client: TelegramClient,
    config: Arc<Mutex<TelegramConfig>>,
    store: TelegramConfigStore,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    let mut offset = {
        let guard = config.lock().await;
        guard.last_update_id.map(|id| id + 1)
    };

    loop {
        // Check shutdown before starting a new poll
        if *shutdown_rx.borrow() {
            break;
        }

        // Race between get_updates and shutdown signal
        let updates_result = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                break;
            }
            result = client.get_updates(offset, POLL_TIMEOUT_SECS) => result,
        };

        match updates_result {
            Ok(updates) => {
                backoff = Duration::from_secs(1);
                if updates.is_empty() {
                    continue;
                }

                for update in updates {
                    offset = Some(update.update_id + 1);
                    if let Err(err) = handle_update(&client, &config, &store, update).await {
                        eprintln!("Telegram update error: {err}");
                    }
                }

                if let Some(next_offset) = offset {
                    let mut guard = config.lock().await;
                    guard.last_update_id = Some(next_offset - 1);
                    store.save(&guard)?;
                }
            }
            Err(err) => {
                eprintln!("Telegram getUpdates failed: {err}");

                // Backoff sleep with shutdown check
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }

                backoff = std::cmp::min(backoff.saturating_mul(2), Duration::from_secs(30));
            }
        }
    }

    Ok(())
}

async fn handle_update(
    client: &TelegramClient,
    config: &Arc<Mutex<TelegramConfig>>,
    store: &TelegramConfigStore,
    update: Update,
) -> Result<()> {
    let message = extract_message(update);
    let Some(message) = message else {
        return Ok(());
    };

    // Get text from either `text` or `caption` (media messages use caption)
    let text = message
        .text
        .clone()
        .or_else(|| message.caption.clone())
        .unwrap_or_default();

    // Extract media info if present
    let media_info = extract_media_info(&message);

    // If no text and no media, skip
    if text.is_empty() && media_info.is_none() {
        return Ok(());
    }

    let Some(from) = &message.from else {
        return Ok(());
    };

    let (chat_id, owner_user_id, agent_name, topic_map) = {
        let guard = config.lock().await;
        (
            guard.chat_id,
            guard.owner_user_id,
            guard.agent_name.clone(),
            guard.channel_topics.clone(),
        )
    };

    if message.chat.id != chat_id {
        return Ok(());
    }

    if from.id != owner_user_id {
        return Ok(());
    }

    let Some(thread_id) = message.message_thread_id else {
        client
            .send_message(chat_id, None, "Please post in a topic.")
            .await?;
        return Ok(());
    };

    // Only parse commands from text messages (not captions)
    if message.text.is_some()
        && let Some(command) = parse_command(&text)
    {
        return handle_command(client, config, store, chat_id, thread_id, command).await;
    }

    let channel = channel_for_topic(&topic_map, thread_id);
    let Some(channel) = channel else {
        client
            .send_message(
                chat_id,
                Some(thread_id),
                "No channel mapped to this topic yet.",
            )
            .await?;
        return Ok(());
    };

    // Validate incoming text
    if !text.is_empty()
        && let Some(error_msg) = validate_incoming_message(&text)
    {
        client
            .send_message(chat_id, Some(thread_id), &error_msg)
            .await?;
        return Ok(());
    }

    // Download media attachment if present
    let mut attachments = Vec::new();
    if let Some(info) = media_info {
        match download_telegram_media(client, &info, &channel).await {
            Ok(attachment) => {
                attachments.push(attachment);
            }
            Err(err) => {
                eprintln!(
                    "Failed to download Telegram media (file_id={}): {err}",
                    info.file_id
                );
                // Continue without attachment rather than dropping the message
            }
        }
    }

    // Build the message body (use text or a default for media-only messages)
    let body = if text.is_empty() {
        "[attachment]".to_string()
    } else {
        text
    };

    // Send to bus using the lower-level send function
    send::run_with_attachments(
        channel,
        body,
        None,
        vec!["human".to_string(), "telegram".to_string()],
        attachments,
        false,
        Some(&agent_name),
    )?;
    Ok(())
}

/// Media info extracted from a Telegram message.
struct MediaInfo {
    file_id: String,
    file_unique_id: String,
    filename: String,
}

/// Extract the best media info from a Telegram message.
///
/// Priority: document > video > audio > animation > voice > photo
/// (Document is preferred because it preserves original filename.)
fn extract_media_info(msg: &TelegramMessage) -> Option<MediaInfo> {
    // Document (preserves original filename)
    if let Some(doc) = &msg.document {
        return Some(MediaInfo {
            file_id: doc.file_id.clone(),
            file_unique_id: doc.file_unique_id.clone(),
            filename: doc
                .file_name
                .clone()
                .unwrap_or_else(|| "document".to_string()),
        });
    }

    // Video
    if let Some(video) = &msg.video {
        return Some(MediaInfo {
            file_id: video.file_id.clone(),
            file_unique_id: video.file_unique_id.clone(),
            filename: video
                .file_name
                .clone()
                .unwrap_or_else(|| "video.mp4".to_string()),
        });
    }

    // Audio
    if let Some(audio) = &msg.audio {
        return Some(MediaInfo {
            file_id: audio.file_id.clone(),
            file_unique_id: audio.file_unique_id.clone(),
            filename: audio
                .file_name
                .clone()
                .unwrap_or_else(|| "audio.mp3".to_string()),
        });
    }

    // Animation (GIF)
    if let Some(anim) = &msg.animation {
        return Some(MediaInfo {
            file_id: anim.file_id.clone(),
            file_unique_id: anim.file_unique_id.clone(),
            filename: anim
                .file_name
                .clone()
                .unwrap_or_else(|| "animation.mp4".to_string()),
        });
    }

    // Voice
    if let Some(voice) = &msg.voice {
        return Some(MediaInfo {
            file_id: voice.file_id.clone(),
            file_unique_id: voice.file_unique_id.clone(),
            filename: "voice.ogg".to_string(),
        });
    }

    // Photo (pick the largest resolution)
    if !msg.photo.is_empty() {
        let best = msg
            .photo
            .iter()
            .max_by_key(|p| p.file_size.unwrap_or(0))
            .unwrap();
        return Some(MediaInfo {
            file_id: best.file_id.clone(),
            file_unique_id: best.file_unique_id.clone(),
            filename: "photo.jpg".to_string(),
        });
    }

    None
}

/// Download a Telegram media file and store it in the attachment cache.
async fn download_telegram_media(
    client: &TelegramClient,
    info: &MediaInfo,
    channel: &str,
) -> Result<Attachment> {
    let bytes = client.download_file(&info.file_id).await?;

    let cache = AttachmentCache::new(attachments_dir())?;
    let stored = cache.store(
        &bytes,
        &info.filename,
        AttachmentSource::Telegram {
            file_id: info.file_id.clone(),
            file_unique_id: info.file_unique_id.clone(),
            message_id: String::new(), // Not yet assigned
            channel: channel.to_string(),
        },
    )?;

    Ok(Attachment {
        name: info.filename.clone(),
        content: AttachmentContent::File {
            path: stored.path.to_string_lossy().to_string(),
        },
    })
}

async fn handle_command(
    client: &TelegramClient,
    config: &Arc<Mutex<TelegramConfig>>,
    store: &TelegramConfigStore,
    chat_id: i64,
    thread_id: i64,
    command: String,
) -> Result<()> {
    let reply = {
        let mut guard = config.lock().await;
        let channel = channel_for_topic(&guard.channel_topics, thread_id)
            .unwrap_or_else(|| "unknown".to_string());

        let (reply, should_save) = match command.as_str() {
            "mute" => {
                if guard.muted_topics.insert(thread_id) {
                    (
                        format!("Muted Telegram notifications for #{channel}."),
                        true,
                    )
                } else {
                    (format!("Already muted for #{channel}."), false)
                }
            }
            "unmute" => {
                if guard.muted_topics.remove(&thread_id) {
                    (
                        format!("Unmuted Telegram notifications for #{channel}."),
                        true,
                    )
                } else {
                    (format!("Not muted for #{channel}."), false)
                }
            }
            _ => ("Unknown command. Use /mute or /unmute.".to_string(), false),
        };

        if should_save {
            store.save(&guard)?;
        }

        reply
    };

    client
        .send_message(chat_id, Some(thread_id), &reply)
        .await?;
    Ok(())
}

fn extract_message(update: Update) -> Option<TelegramMessage> {
    update.message.or(update.edited_message)
}

fn parse_command(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    if !first.starts_with('/') {
        return None;
    }

    let raw = first.trim_start_matches('/');
    let command = raw.split('@').next().unwrap_or(raw);
    if command.is_empty() {
        None
    } else {
        Some(command.to_string())
    }
}

async fn watch_loop(
    client: TelegramClient,
    config: Arc<Mutex<TelegramConfig>>,
    store: TelegramConfigStore,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut offsets = baseline_existing_channel_offsets()?;
    let mut active = active_channels_set()?;

    let mut last_sync = tokio::time::Instant::now();
    let mut interval = tokio::time::interval(WATCH_INTERVAL);

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => break,
            _ = interval.tick() => {}
        }

        // Periodic topic sync
        if last_sync.elapsed() >= SYNC_INTERVAL {
            let mut guard = config.lock().await;
            if sync_topics(&client, &mut guard).await? {
                store.save(&guard)?;
            }
            drop(guard);

            last_sync = tokio::time::Instant::now();
        }

        // Check for new channels and messages
        let current_channels = active_channels_set()?;

        for channel in &current_channels {
            if !is_eligible_channel(channel) {
                continue;
            }

            // New channel appeared
            if !active.contains(channel) {
                if is_channel_closed(channel)? {
                    continue;
                }

                active.insert(channel.clone());
                offsets.entry(channel.clone()).or_insert(0);
                ensure_topic_for_channel(&client, &config, &store, channel).await?;
            }

            // Check for new messages
            let path = channel_path(channel);
            let offset = offsets.get(channel).copied().unwrap_or(0);
            match read_messages_from_offset(&path, offset) {
                Ok((messages, new_offset)) => {
                    for msg in &messages {
                        if let Err(err) = publish_message(&client, &config, &store, msg).await {
                            eprintln!("Failed to publish to Telegram: {err}");
                        }
                    }
                    offsets.insert(channel.clone(), new_offset);
                }
                Err(err) => {
                    eprintln!("Failed to read channel updates: {err}");
                }
            }
        }
    }

    Ok(())
}

async fn publish_message(
    client: &TelegramClient,
    config: &Arc<Mutex<TelegramConfig>>,
    store: &TelegramConfigStore,
    msg: &Message,
) -> Result<()> {
    if !is_eligible_channel(&msg.channel) {
        return Ok(());
    }

    // Skip system messages (hook firings, claims, etc.)
    if is_system_message(msg) {
        return Ok(());
    }

    let (chat_id, agent_name, mut topic_id, mut muted) = {
        let guard = config.lock().await;
        (
            guard.chat_id,
            guard.agent_name.clone(),
            guard.channel_topics.get(&msg.channel).copied(),
            guard
                .channel_topics
                .get(&msg.channel)
                .map(|id| guard.muted_topics.contains(id))
                .unwrap_or(false),
        )
    };

    if msg.agent == agent_name {
        return Ok(());
    }

    if topic_id.is_none() {
        ensure_topic_for_channel(client, config, store, &msg.channel).await?;
        let guard = config.lock().await;
        topic_id = guard.channel_topics.get(&msg.channel).copied();
        muted = guard
            .channel_topics
            .get(&msg.channel)
            .map(|id| guard.muted_topics.contains(id))
            .unwrap_or(false);
    }

    let Some(topic_id) = topic_id else {
        return Ok(());
    };

    if muted {
        return Ok(());
    }

    let text = format_outbound_message(msg);

    // Count file attachments (inline/url attachments are embedded in text)
    let file_attachments: Vec<_> = msg
        .attachments
        .iter()
        .filter(|a| matches!(a.content, AttachmentContent::File { .. }))
        .collect();

    // If we have exactly 1 file attachment and text fits in caption limit,
    // send the file with text as caption (single Telegram message)
    if file_attachments.len() == 1 && text.chars().count() <= TELEGRAM_CAPTION_MAX_CHARS {
        let attachment = file_attachments[0];
        if let Err(err) =
            send_attachment_with_caption(client, chat_id, topic_id, attachment, &text).await
        {
            eprintln!(
                "Failed to send attachment '{}' with caption to Telegram: {err}",
                attachment.name
            );
            // Fall back to separate messages
            client.send_message(chat_id, Some(topic_id), &text).await?;
            let _ = send_attachment_to_telegram(client, chat_id, topic_id, attachment, None).await;
        }
    } else if !file_attachments.is_empty() && text.chars().count() <= TELEGRAM_CAPTION_MAX_CHARS {
        // Multiple file attachments: send first with caption, rest without
        let first = file_attachments[0];
        if let Err(err) =
            send_attachment_with_caption(client, chat_id, topic_id, first, &text).await
        {
            eprintln!(
                "Failed to send attachment '{}' with caption to Telegram: {err}",
                first.name
            );
            // Fall back: send text separately, then all attachments
            client.send_message(chat_id, Some(topic_id), &text).await?;
            for attachment in &file_attachments {
                let _ =
                    send_attachment_to_telegram(client, chat_id, topic_id, attachment, None).await;
            }
        } else {
            // First succeeded, send remaining attachments without caption
            for attachment in file_attachments.iter().skip(1) {
                if let Err(err) =
                    send_attachment_to_telegram(client, chat_id, topic_id, attachment, None).await
                {
                    eprintln!(
                        "Failed to send attachment '{}' to Telegram: {err}",
                        attachment.name
                    );
                }
            }
        }
    } else {
        // No file attachments or text too long for caption: send text first
        client.send_message(chat_id, Some(topic_id), &text).await?;

        // Send file attachments as separate media messages
        for attachment in &file_attachments {
            if let Err(err) =
                send_attachment_to_telegram(client, chat_id, topic_id, attachment, None).await
            {
                eprintln!(
                    "Failed to send attachment '{}' to Telegram: {err}",
                    attachment.name
                );
            }
        }
    }

    Ok(())
}

fn format_outbound_message(msg: &Message) -> String {
    let mut text = format!("{}: {}", msg.agent, msg.body);

    if !msg.labels.is_empty() {
        text.push_str("\nlabels: ");
        text.push_str(&msg.labels.join(", "));
    }

    // Show inline and URL attachments in the text body
    for attachment in &msg.attachments {
        match &attachment.content {
            AttachmentContent::Inline {
                content, language, ..
            } => {
                if content.len() > TELEGRAM_MAX_CHARS / 2 {
                    // Too large for inline display, mention it
                    text.push_str(&format!(
                        "\n[inline: {} ({} bytes)]",
                        attachment.name,
                        content.len()
                    ));
                } else {
                    let lang = language.as_deref().unwrap_or("");
                    text.push_str(&format!("\n```{}\n{}\n```", lang, content));
                }
            }
            AttachmentContent::Url { url } => {
                text.push_str(&format!("\n{}: {}", attachment.name, url));
            }
            AttachmentContent::File { .. } => {
                // File attachments are sent separately as media messages
            }
            AttachmentContent::Unknown(_) => {
                // Written by a newer rite; note its presence without guessing.
                text.push_str(&format!("\n[unsupported attachment: {}]", attachment.name));
            }
        }
    }

    truncate_text(&text, TELEGRAM_MAX_CHARS)
}

/// Send attachment with a custom caption (used when combining text+file into one message).
async fn send_attachment_with_caption(
    client: &TelegramClient,
    chat_id: i64,
    thread_id: i64,
    attachment: &Attachment,
    caption: &str,
) -> Result<()> {
    send_attachment_to_telegram(client, chat_id, thread_id, attachment, Some(caption)).await
}

/// Send a single attachment to Telegram, routing by MIME type.
async fn send_attachment_to_telegram(
    client: &TelegramClient,
    chat_id: i64,
    thread_id: i64,
    attachment: &Attachment,
    caption: Option<&str>,
) -> Result<()> {
    match &attachment.content {
        AttachmentContent::File { path } => {
            let file_path = Path::new(path);
            if !file_path.exists() {
                eprintln!("Attachment file not found: {}", path);
                return Ok(());
            }

            // Read file and detect MIME type
            let bytes = std::fs::read(file_path)?;
            let mime_type = infer::get(&bytes)
                .map(|t| t.mime_type())
                .unwrap_or("application/octet-stream");

            // Use provided caption, or fall back to attachment name
            let caption = caption.or(Some(attachment.name.as_str()));

            // Route based on MIME type
            match mime_type {
                "image/jpeg" | "image/png" | "image/webp" | "image/gif" => {
                    client
                        .send_photo(chat_id, Some(thread_id), file_path, caption)
                        .await?;
                }
                "video/mp4" | "video/webm" | "video/x-matroska" => {
                    client
                        .send_video(chat_id, Some(thread_id), file_path, caption)
                        .await?;
                }
                "audio/mpeg" | "audio/wav" | "audio/flac" | "audio/ogg" => {
                    client
                        .send_audio(chat_id, Some(thread_id), file_path, caption)
                        .await?;
                }
                _ => {
                    // Everything else goes as a document
                    client
                        .send_document(chat_id, Some(thread_id), file_path, caption)
                        .await?;
                }
            }
        }
        AttachmentContent::Inline { content, .. } => {
            // Large inline content: send as a document
            if content.len() > TELEGRAM_MAX_CHARS / 2 {
                let tmp_dir = std::env::temp_dir();
                let tmp_path = tmp_dir.join(&attachment.name);
                std::fs::write(&tmp_path, content)?;
                let caption = caption.or(Some(attachment.name.as_str()));
                client
                    .send_document(chat_id, Some(thread_id), &tmp_path, caption)
                    .await?;
                let _ = std::fs::remove_file(&tmp_path);
            }
            // Small inline content is already embedded in the text message
        }
        AttachmentContent::Url { .. } => {
            // URLs are embedded in the text message body
        }
        AttachmentContent::Unknown(_) => {
            // Unrecognized attachment type; already noted in the text body.
        }
    }

    Ok(())
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let truncated: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{}...", truncated)
}

fn channel_for_topic(map: &HashMap<String, i64>, thread_id: i64) -> Option<String> {
    map.iter().find_map(|(channel, id)| {
        if *id == thread_id {
            Some(channel.clone())
        } else {
            None
        }
    })
}

fn active_channels_set() -> Result<HashSet<String>> {
    let channels = list_active_channels()?;
    Ok(channels.into_iter().collect())
}

fn baseline_existing_channel_offsets() -> Result<HashMap<String, u64>> {
    let channels = list_existing_eligible_channels()?;
    Ok(channels
        .into_iter()
        .map(|channel| {
            let len = channel_len(&channel);
            (channel, len)
        })
        .collect())
}

fn list_existing_eligible_channels() -> Result<Vec<String>> {
    let dir = channels_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut channels = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Some(name) = path.file_stem().and_then(|s| s.to_str())
            && is_eligible_channel(name)
        {
            channels.push(name.to_string());
        }
    }

    channels.sort();
    Ok(channels)
}

fn list_active_channels() -> Result<Vec<String>> {
    let dir = channels_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let closed = ProjectState::new(state_path()).load()?.closed_channels;
    let closed_set: HashSet<String> = closed.into_iter().collect();

    let mut channels = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Some(name) = path.file_stem().and_then(|s| s.to_str())
            && !closed_set.contains(name)
            && is_eligible_channel(name)
        {
            channels.push(name.to_string());
        }
    }

    channels.sort();
    Ok(channels)
}

fn is_channel_closed(channel: &str) -> Result<bool> {
    let state = ProjectState::new(state_path()).load()?;
    Ok(state.closed_channels.iter().any(|c| c == channel))
}

fn channel_len(channel: &str) -> u64 {
    std::fs::metadata(channel_path(channel))
        .map(|m| m.len())
        .unwrap_or(0)
}

async fn ensure_topic_for_channel(
    client: &TelegramClient,
    config: &Arc<Mutex<TelegramConfig>>,
    store: &TelegramConfigStore,
    channel: &str,
) -> Result<()> {
    if !is_eligible_channel(channel) {
        return Ok(());
    }

    let mut guard = config.lock().await;
    if guard.channel_topics.contains_key(channel) {
        return Ok(());
    }

    match client.create_forum_topic(guard.chat_id, channel).await {
        Ok(topic_id) => {
            guard.channel_topics.insert(channel.to_string(), topic_id);
            guard.topic_titles.insert(topic_id, channel.to_string());
            store.save(&guard)?;
        }
        Err(err) => {
            eprintln!("Failed to create Telegram topic for #{channel}: {err}");
        }
    }

    Ok(())
}

async fn sync_topics(client: &TelegramClient, config: &mut TelegramConfig) -> Result<bool> {
    let channels = list_active_channels()?;
    let active: HashSet<String> = channels.iter().cloned().collect();
    let mut changed = false;

    for channel in &channels {
        match config.channel_topics.get(channel).copied() {
            Some(topic_id) => match config.topic_titles.get(&topic_id) {
                Some(title) if title == channel => {}
                None => {
                    config.topic_titles.insert(topic_id, channel.to_string());
                    changed = true;
                }
                _ => {
                    if let Err(err) = client
                        .edit_forum_topic(config.chat_id, topic_id, channel)
                        .await
                        && !is_topic_not_modified_error(&err)
                    {
                        eprintln!("Failed to rename Telegram topic for #{channel}: {err}");
                    } else {
                        config.topic_titles.insert(topic_id, channel.to_string());
                        changed = true;
                    }
                }
            },
            None => match client.create_forum_topic(config.chat_id, channel).await {
                Ok(topic_id) => {
                    config.channel_topics.insert(channel.clone(), topic_id);
                    config.topic_titles.insert(topic_id, channel.to_string());
                    changed = true;
                }
                Err(err) => {
                    eprintln!("Failed to create Telegram topic for #{channel}: {err}");
                }
            },
        }
    }

    let removed_topics: Vec<i64> = config
        .channel_topics
        .iter()
        .filter(|(channel, _)| !active.contains(*channel))
        .map(|(_, topic_id)| *topic_id)
        .collect();

    if !removed_topics.is_empty() {
        config
            .channel_topics
            .retain(|channel, _| active.contains(channel));
        config
            .muted_topics
            .retain(|topic_id| !removed_topics.contains(topic_id));
        config
            .topic_titles
            .retain(|topic_id, _| !removed_topics.contains(topic_id));
        changed = true;
    }

    Ok(changed)
}

fn is_eligible_channel(channel: &str) -> bool {
    !channel.starts_with(SYSTEM_CHANNEL_PREFIX)
}

/// Validate incoming Telegram message. Returns error message if invalid, None if valid.
fn validate_incoming_message(text: &str) -> Option<String> {
    // Check length limit to prevent memory exhaustion
    if text.len() > MAX_INCOMING_MESSAGE_LEN {
        return Some(format!(
            "Message too long ({} bytes, max {} bytes)",
            text.len(),
            MAX_INCOMING_MESSAGE_LEN
        ));
    }

    // Check for null bytes which could cause issues in downstream processing
    if text.contains('\0') {
        return Some("Message contains invalid characters (null bytes)".to_string());
    }

    None
}

fn is_topic_not_modified_error(err: &anyhow::Error) -> bool {
    let message = err.to_string().to_lowercase();
    message.contains("not modified") || message.contains("not_modified")
}

/// Check if a message is a system message (hook firings, claims, etc.)
fn is_system_message(msg: &Message) -> bool {
    matches!(
        &msg.meta,
        Some(
            MessageMeta::System { .. }
                | MessageMeta::Claim { .. }
                | MessageMeta::ClaimExtended { .. }
                | MessageMeta::Release { .. }
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::project::{DATA_DIR_ENV_VAR, ensure_data_dir};
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TestEnv {
        _dir: TempDir,
    }

    impl TestEnv {
        fn new() -> Self {
            Self::with_data_dir(true)
        }

        fn without_data_dir() -> Self {
            Self::with_data_dir(false)
        }

        fn with_data_dir(create_data_dir: bool) -> Self {
            let dir = TempDir::new().unwrap();
            unsafe {
                env::set_var(DATA_DIR_ENV_VAR, dir.path());
            }

            if create_data_dir {
                ensure_data_dir().unwrap();
            }

            Self { _dir: dir }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                env::remove_var(DATA_DIR_ENV_VAR);
            }
        }
    }

    #[test]
    #[serial]
    fn baseline_offsets_do_not_require_channels_dir() {
        let _env = TestEnv::without_data_dir();

        let offsets = baseline_existing_channel_offsets().unwrap();

        assert!(offsets.is_empty());
    }

    #[test]
    #[serial]
    fn baseline_offsets_start_existing_channels_at_end() {
        let _env = TestEnv::new();
        let message = Message::new("alice", "general", "already relayed");
        crate::storage::jsonl::append_record(&channel_path("general"), &message).unwrap();

        let offsets = baseline_existing_channel_offsets().unwrap();
        let expected_len = channel_len("general");

        assert_eq!(offsets.get("general").copied(), Some(expected_len));
    }

    #[test]
    #[serial]
    fn baseline_offsets_skip_system_channels() {
        let _env = TestEnv::new();
        std::fs::write(channel_path("_dm_alice_bob"), "{}\n").unwrap();

        let offsets = baseline_existing_channel_offsets().unwrap();

        assert!(!offsets.contains_key("_dm_alice_bob"));
    }
}
