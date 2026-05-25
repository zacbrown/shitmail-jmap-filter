use chrono::Utc;
use serde_json::{json, Map, Value};
use std::io::{self, Write};

pub fn emit(level: &str, event: &str, fields: Map<String, Value>) {
    let mut record = Map::new();
    record.insert("ts".into(), json!(Utc::now().to_rfc3339()));
    record.insert("level".into(), json!(level));
    record.insert("event".into(), json!(event));
    for (k, v) in fields {
        record.insert(k, v);
    }
    let line = serde_json::to_string(&Value::Object(record)).unwrap_or_else(|_| String::from(r#"{"level":"error","event":"log.encode_failed"}"#));
    let stdout = io::stdout();
    let mut h = stdout.lock();
    let _ = writeln!(h, "{}", line);
}

#[macro_export]
macro_rules! log_event {
    ($level:expr, $event:expr $(, $key:tt => $value:expr)* $(,)?) => {{
        #[allow(unused_mut)]
        let mut fields = serde_json::Map::new();
        $( fields.insert(stringify!($key).trim_matches('"').to_string(), serde_json::json!($value)); )*
        $crate::log::emit($level, $event, fields);
    }};
}
