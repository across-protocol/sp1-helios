use chrono::Local;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tokio::task;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::{extract_message, run_id};

// Prune finished Slack tasks only when the list grows past this threshold
const SLACK_PRUNE_THRESHOLD: usize = 16;

// Track spawned Slack posting tasks so we can flush them on shutdown
static SLACK_TASK_HANDLES: Lazy<Mutex<Vec<JoinHandle<()>>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Serialize)]
struct SlackPayload {
    text: String,
}

pub struct SlackLayer {
    client: Client,
    webhook: String,
    threshold: Level,
}

impl SlackLayer {
    /// webhook: your Slack incoming-webhook URL
    /// threshold: only events >= this level are sent
    pub fn new(webhook: impl Into<String>, threshold: Level) -> Self {
        SlackLayer {
            client: Client::new(),
            webhook: webhook.into(),
            threshold,
        }
    }
}

impl<S> Layer<S> for SlackLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event, _ctx: Context<S>) {
        let metadata = event.metadata();
        let level = *metadata.level();
        let target = metadata.target();

        // Basic level thresholding (send when at or above threshold)
        let meets_level_treshold = level <= self.threshold;

        // Additional inclusion rules for noisy-but-useful SP1 logs
        let mut message = extract_message(event);
        let is_relevant_generate_event =
            target == "proof_service::generate" && level <= Level::DEBUG;
        let is_useful_sp1_event = level == Level::INFO
            && (message.starts_with("Created request")
                || message.starts_with("View request status at:")
                || message.starts_with("Proof request assigned"));
        let is_main_loop_start_event = level == Level::INFO
            && target == "proof_service::run"
            && message.starts_with("Starting main loop");

        let should_send = meets_level_treshold
            || is_relevant_generate_event
            || is_useful_sp1_event
            || is_main_loop_start_event;

        if !should_send {
            return;
        }

        // Filter out specific warnings
        if level == Level::WARN
            && target == "helios_consensus_core::consensus_core"
            && message.starts_with("skipping block with low vote count")
        {
            return;
        }

        if message.is_empty() {
            message = "Event occurred".to_string();
        }

        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");

        // Map level to a Slack-friendly emoji
        let level_emoji = match level {
            Level::ERROR => ":x:",
            Level::WARN => ":warning:",
            Level::INFO => ":white_check_mark:",
            Level::DEBUG => ":mag:",
            Level::TRACE => ":mag:",
        };

        // Old-style Slack line with backticks and rearranged fields:
        // `[ts] [LEVEL]` :emoji: `[run_id::target]` message
        let text = format!(
            "`[{}] [{:<5}]` {} `[{}::{}]` {}",
            ts,
            level,
            level_emoji,
            run_id(),
            target,
            message,
        );

        // Fire-and-forget async post
        let client = self.client.clone();
        let url = self.webhook.clone();
        let handle = task::spawn(async move {
            match client.post(&url).json(&SlackPayload { text }).send().await {
                Ok(response) => {
                    let status = response.status();
                    if !status.is_success() {
                        if let Ok(body) = response.text().await {
                            eprintln!(
                                "SlackLayer Error: Failed sending to Slack (Status {}): {}",
                                status, body
                            );
                        } else {
                            eprintln!(
                                "SlackLayer Error: Failed sending to Slack (Status {}): Response body read failed",
                                status
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("SlackLayer Error: Request failed: {}", e);
                }
            }
        });

        if let Ok(mut v) = SLACK_TASK_HANDLES.lock() {
            v.push(handle);
            if v.len() >= SLACK_PRUNE_THRESHOLD {
                v.retain(|h| !h.is_finished());
                if v.capacity() > v.len() * 2 {
                    v.shrink_to_fit();
                }
            }
        }
    }
}

/// Flushes all in-flight Slack send tasks by waiting for the task tracker to drain.
pub async fn flush() {
    let maybe_handles: Option<Vec<JoinHandle<()>>> = SLACK_TASK_HANDLES
        .lock()
        .ok()
        .map(|mut v| v.drain(..).collect());

    match maybe_handles {
        Some(handles) => {
            for handle in handles {
                let _ = handle.await;
            }
        }
        None => {
            // Fallback: if we weren't able to lock the mutex, just wait before exiting
            sleep(Duration::from_secs(5)).await;
        }
    }
}
