// slack_layer.rs
use chrono::Local;
use reqwest::Client;
use serde::Serialize;
use tokio::task;
use tracing::field::Field;
use tracing::{field::Visit, Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

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
        let relevant_event = event.metadata().target() == "proof_service::generate"
            || *event.metadata().level() <= self.threshold;
        if !relevant_event {
            return;
        }

        let mut message = String::new();
        let mut visitor = MessageExtractor {
            message_buf: &mut message,
        };
        event.record(&mut visitor);

        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let level = event.metadata().level();
        let target = event.metadata().target();

        // Filter out specific warnings
        if *level == Level::WARN
            && target == "helios_consensus_core::consensus_core"
            && message.starts_with("skipping block with low vote count")
        {
            return;
        }

        let level_emoji = match *level {
            Level::ERROR => "🚨",
            Level::WARN => "⚠️",
            Level::INFO => "ℹ️",
            Level::DEBUG => "🐛",
            Level::TRACE => "👣",
        };

        let final_text = if message.is_empty() {
            format!(
                "`{} [{:<5}]` {} `[{}]` Event occurred (no message field)",
                ts, level, level_emoji, target
            )
        } else {
            format!(
                "`{} [{:<5}]` {} `[{}]` {}",
                ts, level, level_emoji, target, message
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
