// slack_layer.rs
use chrono::Local;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::Serialize;
use tokio::task;
use tracing::field::Field;
use tracing::{field::Visit, Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

// Generate a unique run ID once
static RUN_ID: Lazy<String> = Lazy::new(|| {
    let full_uuid = Uuid::new_v4().to_string();
    // Take the first 6 hex characters
    full_uuid.chars().take(6).collect()
});

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

// Visitor to extract only the message field
struct MessageExtractor<'a> {
    message_buf: &'a mut String,
}

impl Visit for MessageExtractor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" && self.message_buf.is_empty() {
            self.message_buf.push_str(value);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && self.message_buf.is_empty() {
            use std::fmt::Write;
            let _ = write!(self.message_buf, "{:?}", value);
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

        let mut message = String::new();
        let mut visitor = MessageExtractor {
            message_buf: &mut message,
        };
        event.record(&mut visitor);

        let is_relevant_generate_event =
            target == "proof_service::generate" && level <= Level::DEBUG;
        let is_below_threshold = level <= self.threshold;

        // todo? A not so pretty way to catch some of the sp1-specific logs
        let is_useful_sp1_event = level == Level::INFO
            && (message.starts_with("Created request")
                || message.starts_with("View request status at:")
                || message.starts_with("Proof request assigned"));

        // Explicitly include the main loop start message
        let is_main_loop_start_event = level == Level::INFO
            && target == "proof_service::run"
            && message.starts_with("Starting main loop");

        let relevant_event = is_relevant_generate_event
            || is_below_threshold
            || is_useful_sp1_event
            || is_main_loop_start_event;

        if !relevant_event {
            return;
        }

        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");

        // Filter out specific warnings
        if level == Level::WARN
            && target == "helios_consensus_core::consensus_core"
            && message.starts_with("skipping block with low vote count")
        {
            return;
        }

        let level_emoji = match level {
            Level::ERROR => "❌",
            Level::WARN => "⚠️",
            Level::INFO => "✅",
            Level::DEBUG => "🛠️",
            Level::TRACE => "👣",
        };

        // Override the target for specific SP1 SDK logs for better clarity
        let display_target = if is_useful_sp1_event {
            "sp1_sdk"
        } else {
            target
        };

        let final_text = if message.is_empty() {
            format!(
                "`[{}] [{:<5}]` {} `[{}::{}]` Event occurred (no message field)",
                ts, level, level_emoji, *RUN_ID, display_target
            )
        } else {
            format!(
                "`[{}] [{:<5}]` {} `[{}::{}]` {}",
                ts, level, level_emoji, *RUN_ID, display_target, message
            )
        };

        // Fire-and-forget async post
        let client = self.client.clone();
        let url = self.webhook.clone();
        let text = final_text;
        task::spawn(async move {
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
                            eprintln!("SlackLayer Error: Failed sending to Slack (Status {}): Response body read failed", status);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("SlackLayer Error: Request failed: {}", e);
                }
            }
        });
    }
}
