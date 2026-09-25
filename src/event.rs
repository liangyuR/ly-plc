use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::engine::{PlcStatus, PointValue};
use crate::logbook::LogEntry;
use crate::value::PlcValue;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeEvent {
    pub point_id: String,
    pub point_name: String,
    pub tags: Vec<String>,
    pub rising: bool,
    pub value: PlcValue,
    pub ts: i64,
}

pub enum PlcEvent {
    Status(PlcStatus),
    Values(HashMap<String, PointValue>),
    Logs(Vec<LogEntry>),
    Edge(EdgeEvent),
}

pub type EventSink = Arc<dyn Fn(PlcEvent) + Send + Sync>;
