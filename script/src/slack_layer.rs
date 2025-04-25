// slack_layer.rs
use chrono::Local;
use reqwest::Client;
use serde::Serialize;
use tokio::task;
use tracing::field::Field;
// use tracing::tracing_core::field::Field;
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

impl<'a> Visit for MessageExtractor<'a> {
    // Only care about string and debug representations for the message
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" && self.message_buf.is_empty() {
            // Take the first "message" field
            self.message_buf.push_str(value);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && self.message_buf.is_empty() {
            // Take the first "message" field
            use std::fmt::Write;
            let _ = write!(self.message_buf, "{:?}", value);
        }
    }
    // Ignore bool, i64, u64, etc.
}

impl<S> Layer<S> for SlackLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event, _ctx: Context<S>) {
        // Context not needed here
        // 1) Level filter: Reject events LESS severe than the threshold
        if *event.metadata().level() > self.threshold {
            // Example: If threshold is WARN, and event is INFO, INFO > WARN is true, so reject.
            // Example: If threshold is WARN, and event is WARN, WARN > WARN is false, so keep.
            // Example: If threshold is WARN, and event is ERROR, ERROR > WARN is false, so keep.
            return;
        }

        // If we reach here, the event level is <= self.threshold (equally or more severe)

        // 2) Extract the message field
        let mut message = String::new();
        let mut visitor = MessageExtractor {
            message_buf: &mut message,
        };
        event.record(&mut visitor);

        // 3) Timestamp and manually format the output string
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let level = event.metadata().level();
        let target = event.metadata().target();

        let final_text = if message.is_empty() {
            // Fallback if no message field found
            format!(
                "{} [{:<5}] [{}] Event occurred (no message field)",
                ts, level, target
            )
        } else {
            // Format with the extracted message
            format!("{} [{:<5}] [{}] {}", ts, level, target, message)
        };

        // 4) Fire-and-forget async post (with fixed error handling)
        let client = self.client.clone();
        let url = self.webhook.clone();
        let text = final_text; // Use the manually formatted string
        task::spawn(async move {
            match client.post(&url).json(&SlackPayload { text }).send().await {
                Ok(response) => {
                    let status = response.status(); // Get status before consuming body
                    if !status.is_success() {
                        if let Ok(body) = response.text().await {
                            // Consume body only on error
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
