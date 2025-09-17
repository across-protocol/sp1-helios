use chrono::{Local, Utc};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::fmt::Write as FmtWrite;
use std::str::FromStr;
use tracing::Level;
use tracing_subscriber::fmt::{self, format::Writer, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub mod slack;

// Shared short run identifier for this process, used by all layers
static RUN_ID: Lazy<String> = Lazy::new(|| {
    use uuid::Uuid;
    Uuid::new_v4().to_string().chars().take(6).collect()
});

pub fn run_id() -> &'static str {
    &RUN_ID
}

// Extract only the formatted "message" field from a tracing event
pub(crate) fn extract_message(event: &tracing::Event<'_>) -> String {
    struct OnlyMessage<'a>(&'a mut String);

    impl<'a> tracing::field::Visit for OnlyMessage<'a> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" && self.0.is_empty() {
                self.0.push_str(value);
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" && self.0.is_empty() {
                let _ = write!(self.0, "{:?}", value);
            }
        }
    }

    let mut msg = String::new();
    event.record(&mut OnlyMessage(&mut msg));
    msg
}

// Visitor that captures all event fields into a JSON map so we don't lose information
struct JsonFieldsVisitor {
    fields: Map<String, Value>,
}

impl JsonFieldsVisitor {
    fn new() -> Self {
        Self { fields: Map::new() }
    }
}

impl tracing::field::Visit for JsonFieldsVisitor {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        self.fields
            .insert(field.name().to_string(), Value::from(value.to_string()));
    }
    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        self.fields
            .insert(field.name().to_string(), Value::from(value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), Value::from(value.to_string()));
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.fields
            .insert(field.name().to_string(), Value::from(value.to_string()));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            Value::from(format!("{:?}", value)),
        );
    }
}

// Minimal GCP/Cloud Logging JSON formatter: writes top-level time, severity, message, target, run_id, plus all event fields
struct GcpJson;

impl<S, N> FormatEvent<S, N> for GcpJson
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let ts = Utc::now().to_rfc3339();
        let sev = match *event.metadata().level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARNING",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "DEBUG",
        };
        let target = event.metadata().target();
        let level = event.metadata().level().to_string();

        // Capture event fields
        let mut visitor = JsonFieldsVisitor::new();
        event.record(&mut visitor);

        // Extract message if present
        let message = visitor
            .fields
            .remove("message")
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_else(|| extract_message(event));

        // Collect current span chain for additional context
        let spans: Vec<String> = ctx
            .lookup_current()
            .into_iter()
            .flat_map(|span| {
                span.scope()
                    .from_root()
                    .map(|s| s.metadata().name().to_string())
            })
            .collect();

        // Include file/line/module when available
        let file = event.metadata().file();
        let line = event.metadata().line();
        let module_path = event.metadata().module_path();

        let mut top = Map::new();
        top.insert("time".to_string(), Value::from(ts));
        top.insert("severity".to_string(), Value::from(sev));
        top.insert("level".to_string(), Value::from(level));
        top.insert("target".to_string(), Value::from(target.to_string()));
        top.insert("run_id".to_string(), Value::from(run_id().to_string()));
        if let Some(f) = file {
            top.insert("file".to_string(), Value::from(f.to_string()));
        }
        if let Some(l) = line {
            top.insert("line".to_string(), Value::from(l));
        }
        if let Some(mp) = module_path {
            top.insert("module_path".to_string(), Value::from(mp.to_string()));
        }
        if !spans.is_empty() {
            top.insert("spans".to_string(), Value::from(spans));
        }
        top.insert("message".to_string(), Value::from(message));

        // Flatten all event fields (excluding message which we already set)
        for (k, v) in visitor.fields.into_iter() {
            top.insert(k, v);
        }

        let out = Value::Object(top);
        writeln!(
            w,
            "{}",
            serde_json::to_string(&out)
                .unwrap_or_else(|_| json!({"message": "<json-serde-error>"}).to_string())
        )
    }
}

// Compact console formatter to keep Slack and console messages consistent
struct ConsoleCompact;

impl<S, N> FormatEvent<S, N> for ConsoleCompact
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let level = event.metadata().level();
        let target = event.metadata().target();
        let mut msg = extract_message(event);
        if msg.is_empty() {
            // If no explicit message field, fall back to debug-formatting the entire event fields
            let mut visit = JsonFieldsVisitor::new();
            event.record(&mut visit);
            msg = if visit.fields.is_empty() {
                "Event occurred".to_string()
            } else {
                serde_json::to_string(&Value::Object(visit.fields))
                    .unwrap_or_else(|_| "<unformattable event fields>".to_string())
            };
        }
        writeln!(
            w,
            "[{}] [{}] [{:<5}] [{}] {}",
            ts,
            run_id(),
            level,
            target,
            msg
        )
    }
}

pub fn init_tracing() -> anyhow::Result<()> {
    // 1) Read RUST_LOG or default to "info" for console/default filtering
    let default_filter =
        EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?;

    // 2) Detect Cloud Run environment
    let is_cloud_run = std::env::var_os("K_SERVICE").is_some();

    // 3) Base registry with filter
    let base = tracing_subscriber::registry().with(default_filter);

    // 4) Optional Slack layer
    let slack_layer_opt = match std::env::var("SLACK_WEBHOOK_URL") {
        Ok(webhook_url) if !webhook_url.is_empty() => {
            let slack_level_str =
                std::env::var("SLACK_LOG_LEVEL").unwrap_or_else(|_| "WARN".to_string());
            let slack_threshold =
                Level::from_str(&slack_level_str.to_uppercase()).unwrap_or(Level::WARN);
            println!(
                "Initializing Slack tracing layer with webhook URL and level >= {} (run_id={})",
                slack_threshold,
                run_id()
            );
            Some(slack::SlackLayer::new(webhook_url, slack_threshold))
        }
        _ => {
            println!(
                "No SLACK_WEBHOOK_URL found, skipping Slack layer initialization. (run_id={})",
                run_id()
            );
            None
        }
    };

    // 5) Build and init subscriber per environment without unifying types
    if is_cloud_run {
        let fmt_layer = fmt::layer()
            .event_format(GcpJson)
            .with_ansi(false)
            .with_writer(std::io::stdout);

        if let Some(slack_layer) = slack_layer_opt {
            base.with(fmt_layer).with(slack_layer).init();
        } else {
            base.with(fmt_layer).init();
        }
    } else {
        let fmt_layer = fmt::layer()
            .event_format(ConsoleCompact)
            .with_writer(std::io::stdout);

        if let Some(slack_layer) = slack_layer_opt {
            base.with(fmt_layer).with(slack_layer).init();
        } else {
            base.with(fmt_layer).init();
        }
    }

    Ok(())
}
