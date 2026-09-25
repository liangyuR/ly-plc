use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::event::{EdgeEvent, EventSink, PlcEvent};
use crate::logbook::{now_ms, Category, Level, LogEntry, Logbook};
use crate::model::{DataType, EdgeMode, PlcConfig, PlcPoint, WordOrder};
use crate::plan;
use crate::protocol::{
    self, sim::SimMemory, DriverError, Location, Protocol, Region, RegionData, WriteValue,
};
use crate::value::{self, PlcValue};

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlcStatus {
    pub state: LinkState,
    pub message: String,
    pub since: i64,
    pub last_poll: Option<i64>,
    pub cycle_ms: Option<u64>,
    pub poll_count: u64,
    pub error_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PointValue {
    pub value: Option<PlcValue>,
    pub error: Option<String>,
    pub ts: i64,
}

struct CompiledPoint {
    point: PlcPoint,
    loc: Location,
    order: WordOrder,
}

impl CompiledPoint {
    fn encode(&self, value: &Value) -> Result<WriteValue, String> {
        if self.point.data_type == DataType::Bool {
            Ok(WriteValue::Bit(value::parse_bool(value)?))
        } else {
            Ok(WriteValue::Bytes(value::encode(
                value,
                self.point.data_type,
                self.order,
            )?))
        }
    }

    fn decode(&self, region: &Region, data: &RegionData) -> Result<PlcValue, String> {
        let offset = (self.loc.start - region.start) as usize;
        match data {
            RegionData::Bits(bits) => bits
                .get(offset)
                .map(|b| PlcValue::Bool(*b))
                .ok_or_else(|| "数据长度不足".into()),
            RegionData::Bytes(bytes) => {
                let ub = self.loc.area.unit.bytes() as usize;
                let start = offset * ub;
                let slice = bytes
                    .get(start..start + self.loc.len as usize * ub)
                    .ok_or_else(|| "数据长度不足".to_string())?;
                match self.loc.bit {
                    Some(bit) => Ok(PlcValue::Bool(value::decode_bit(
                        slice,
                        self.loc.area.unit,
                        bit,
                    ))),
                    None => value::decode(slice, self.point.data_type, self.order),
                }
            }
        }
    }
}

struct Compiled {
    config: PlcConfig,
    points: Vec<CompiledPoint>,
    heartbeat: Option<(usize, Duration)>,
}

fn compile(config: PlcConfig) -> (Compiled, Vec<String>) {
    let mut errors = Vec::new();
    let mut points = Vec::new();
    let order = protocol::default_order(config.connection.protocol);
    for p in &config.points {
        match protocol::resolve(&config.connection, &p.address, p.data_type) {
            Ok(loc) => points.push(CompiledPoint {
                point: p.clone(),
                loc,
                order: p.word_order.unwrap_or(order),
            }),
            Err(e) => errors.push(format!("点位「{}」已跳过：{e}", p.name)),
        }
    }
    let heartbeat = config
        .heartbeat
        .point_id
        .as_ref()
        .and_then(|id| points.iter().position(|c| &c.point.id == id))
        .map(|i| {
            (
                i,
                Duration::from_millis(config.heartbeat.interval_ms.max(100)),
            )
        });
    (
        Compiled {
            config,
            points,
            heartbeat,
        },
        errors,
    )
}

struct Shared {
    compiled: RwLock<Arc<Compiled>>,
    status: Mutex<PlcStatus>,
    values: Mutex<HashMap<String, PointValue>>,
    driver: tokio::sync::Mutex<Option<Box<dyn Protocol>>>,
    sim: Arc<Mutex<SimMemory>>,
    logbook: Arc<Logbook>,
    sink: EventSink,
}

pub struct PlcEngine {
    shared: Arc<Shared>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Shared {
    fn log(&self, entry: LogEntry) {
        self.logbook.log(entry);
    }

    fn emit(&self, event: PlcEvent) {
        (self.sink)(event);
    }

    fn update_status(&self, f: impl FnOnce(&mut PlcStatus)) {
        let snapshot = {
            let mut s = self.status.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        self.emit(PlcEvent::Status(snapshot));
    }

    fn set_state(&self, state: LinkState, message: String) {
        self.update_status(|s| {
            if s.state != state {
                s.since = now_ms();
            }
            s.state = state;
            s.message = message;
        });
    }

    fn fail(&self, last_error: &mut Option<String>, message: String) {
        self.update_status(|s| {
            if s.state != LinkState::Error {
                s.since = now_ms();
            }
            s.state = LinkState::Error;
            s.message = message.clone();
            s.error_count += 1;
        });
        if last_error.as_deref() != Some(message.as_str()) {
            self.log(LogEntry::new(
                Level::Error,
                Category::Connection,
                message.clone(),
            ));
            *last_error = Some(message);
        }
    }

    async fn poll_once(&self, c: &Compiled, heartbeat: &mut (Instant, bool)) -> Result<(), String> {
        let mut guard = self.driver.lock().await;
        let driver = guard.as_mut().ok_or("PLC 未连接")?;
        driver.tick();
        let locations: Vec<&Location> = c.points.iter().map(|p| &p.loc).collect();
        let blocks = plan::plan(&locations, |u| driver.max_region(u));
        let now = now_ms();
        let mut fresh: HashMap<String, PointValue> = HashMap::with_capacity(c.points.len());

        for block in &blocks {
            let data = match driver.read(&block.region).await {
                Ok(d) => Ok(d),
                Err(DriverError::Link(m)) => return Err(m),
                Err(DriverError::Exception(m)) => Err(m),
            };
            for &i in &block.members {
                let cp = &c.points[i];
                let decoded = match &data {
                    Ok(d) => cp.decode(&block.region, d),
                    Err(m) => Err(m.clone()),
                };
                let pv = match decoded {
                    Ok(v) => PointValue {
                        value: Some(v),
                        error: None,
                        ts: now,
                    },
                    Err(e) => PointValue {
                        value: None,
                        error: Some(e),
                        ts: now,
                    },
                };
                fresh.insert(cp.point.id.clone(), pv);
            }
        }

        if let Some((i, interval)) = c.heartbeat {
            if heartbeat.0.elapsed() >= interval {
                heartbeat.0 = Instant::now();
                heartbeat.1 = !heartbeat.1;
                let cp = &c.points[i];
                if let Ok(v) = cp.encode(&Value::Bool(heartbeat.1)) {
                    if let Err(DriverError::Link(m)) = driver.write(&cp.loc, &v).await {
                        return Err(m);
                    }
                }
            }
        }
        drop(guard);

        self.apply(c, fresh);
        Ok(())
    }

    fn apply(&self, c: &Compiled, mut fresh: HashMap<String, PointValue>) {
        let mut changed: HashMap<String, PointValue> = HashMap::new();
        {
            let mut values = self.values.lock().unwrap();
            for cp in &c.points {
                let Some(new) = fresh.remove(&cp.point.id) else {
                    continue;
                };
                let old = values.get(&cp.point.id);
                if old.is_some_and(|o| o.value == new.value && o.error == new.error) {
                    continue;
                }
                self.record_change(&cp.point, old, &new);
                changed.insert(cp.point.id.clone(), new.clone());
                values.insert(cp.point.id.clone(), new);
            }
            values.retain(|id, _| c.points.iter().any(|p| &p.point.id == id));
        }
        if !changed.is_empty() {
            self.emit(PlcEvent::Values(changed));
        }
    }

    fn record_change(&self, p: &PlcPoint, old: Option<&PointValue>, new: &PointValue) {
        if let Some(err) = &new.error {
            if old.is_none_or(|o| o.error.as_ref() != Some(err)) {
                self.log(
                    LogEntry::new(Level::Warn, Category::Error, format!("读取失败：{err}"))
                        .point(&p.id, &p.name),
                );
            }
            return;
        }
        let Some(nv) = &new.value else { return };
        let ov = old.and_then(|o| o.value.as_ref());
        if p.log_changes {
            let old_text = ov.map(|v| v.display());
            let message = format!("{} → {}", old_text.as_deref().unwrap_or("--"), nv.display());
            self.log(
                LogEntry::new(Level::Info, Category::Value, message)
                    .point(&p.id, &p.name)
                    .values(old_text, Some(nv.display())),
            );
        }
        let Some(ov) = ov else { return };
        let (was, is) = (ov.is_truthy(), nv.is_truthy());
        if was == is {
            return;
        }
        let wanted = match p.edge {
            EdgeMode::None => false,
            EdgeMode::Rising => is,
            EdgeMode::Falling => !is,
            EdgeMode::Both => true,
        };
        if wanted {
            let label = if is { "上升沿" } else { "下降沿" };
            self.log(LogEntry::new(Level::Info, Category::Edge, label).point(&p.id, &p.name));
            self.emit(PlcEvent::Edge(EdgeEvent {
                point_id: p.id.clone(),
                point_name: p.name.clone(),
                tags: p.tags.clone(),
                rising: is,
                value: nv.clone(),
                ts: new.ts,
            }));
        }
    }

    fn mark_offline(&self, reason: &str) {
        let now = now_ms();
        let snapshot: HashMap<String, PointValue> = {
            let mut values = self.values.lock().unwrap();
            for v in values.values_mut() {
                *v = PointValue {
                    value: None,
                    error: Some(reason.into()),
                    ts: now,
                };
            }
            values.clone()
        };
        if !snapshot.is_empty() {
            self.emit(PlcEvent::Values(snapshot));
        }
    }

    async fn drop_driver(&self) {
        if let Some(mut d) = self.driver.lock().await.take() {
            d.close().await;
        }
    }
}

async fn poll_loop(shared: Arc<Shared>) {
    let mut last_error: Option<String> = None;
    let mut heartbeat = (Instant::now(), false);
    let mut last_emit = Instant::now();
    loop {
        let c = shared.compiled.read().unwrap().clone();
        let conn = &c.config.connection;
        let reconnect = Duration::from_millis(conn.reconnect_interval_ms.max(200));
        {
            let mut guard = shared.driver.lock().await;
            if guard.is_none() {
                let target = conn.target();
                shared.set_state(LinkState::Connecting, format!("正在连接 {target}"));
                match protocol::connect(conn, &shared.sim).await {
                    Ok(d) => {
                        *guard = Some(d);
                        let message = if last_error.take().is_some() {
                            format!("重新连接成功 {target}")
                        } else {
                            format!("已连接 {target}")
                        };
                        shared.set_state(LinkState::Connected, message.clone());
                        shared.log(LogEntry::new(Level::Info, Category::Connection, message));
                    }
                    Err(e) => {
                        drop(guard);
                        shared.fail(&mut last_error, e);
                        sleep(reconnect).await;
                        continue;
                    }
                }
            }
        }

        let started = Instant::now();
        if let Err(e) = shared.poll_once(&c, &mut heartbeat).await {
            shared.drop_driver().await;
            shared.mark_offline("通讯中断");
            shared.fail(&mut last_error, e);
            sleep(reconnect).await;
            continue;
        }
        let cycle = started.elapsed();
        {
            let mut s = shared.status.lock().unwrap();
            s.poll_count += 1;
            s.last_poll = Some(now_ms());
            s.cycle_ms = Some(cycle.as_millis() as u64);
        }
        if last_emit.elapsed() >= Duration::from_secs(1) {
            last_emit = Instant::now();
            shared.update_status(|_| {});
        }
        sleep(Duration::from_millis(conn.poll_interval_ms).saturating_sub(cycle)).await;
    }
}

impl PlcEngine {
    pub fn new(config: PlcConfig, logbook: Arc<Logbook>, sink: EventSink) -> Self {
        let (compiled, errors) = compile(config);
        for e in errors {
            logbook.log(LogEntry::new(Level::Warn, Category::Config, e));
        }
        let shared = Shared {
            compiled: RwLock::new(Arc::new(compiled)),
            status: Mutex::new(PlcStatus {
                state: LinkState::Disconnected,
                message: "未连接".into(),
                since: now_ms(),
                last_poll: None,
                cycle_ms: None,
                poll_count: 0,
                error_count: 0,
            }),
            values: Mutex::new(HashMap::new()),
            driver: tokio::sync::Mutex::new(None),
            sim: Arc::new(Mutex::new(SimMemory::default())),
            logbook,
            sink,
        };
        Self {
            shared: Arc::new(shared),
            task: Mutex::new(None),
        }
    }

    pub fn config(&self) -> PlcConfig {
        self.shared.compiled.read().unwrap().config.clone()
    }

    pub fn status(&self) -> PlcStatus {
        self.shared.status.lock().unwrap().clone()
    }

    pub fn values(&self) -> HashMap<String, PointValue> {
        self.shared.values.lock().unwrap().clone()
    }

    pub fn logbook(&self) -> &Arc<Logbook> {
        &self.shared.logbook
    }

    pub fn is_running(&self) -> bool {
        self.task.lock().unwrap().is_some()
    }

    pub async fn apply_config(&self, config: PlcConfig) -> Result<(), String> {
        config.validate()?;
        self.shared.logbook.set_retention(config.log_retention_days);
        let message = format!("配置已更新（{} 个点位）", config.points.len());
        let (compiled, _) = compile(config);
        let connection_changed = {
            let mut current = self.shared.compiled.write().unwrap();
            let changed = current.config.connection != compiled.config.connection;
            *current = Arc::new(compiled);
            changed
        };
        if connection_changed {
            self.shared.drop_driver().await;
        }
        self.shared
            .log(LogEntry::new(Level::Info, Category::Config, message));
        Ok(())
    }

    pub async fn connect(&self) {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return;
        }
        self.shared.update_status(|s| {
            s.poll_count = 0;
            s.error_count = 0;
        });
        *task = Some(tokio::spawn(poll_loop(self.shared.clone())));
    }

    pub async fn disconnect(&self) {
        let handle = self.task.lock().unwrap().take();
        let Some(handle) = handle else { return };
        handle.abort();
        self.shared.drop_driver().await;
        self.shared.values.lock().unwrap().clear();
        self.shared.emit(PlcEvent::Values(HashMap::new()));
        self.shared
            .set_state(LinkState::Disconnected, "已断开".into());
        self.shared.log(LogEntry::new(
            Level::Info,
            Category::Connection,
            "手动断开连接",
        ));
    }

    pub async fn write_point(&self, id: &str, value: &Value) -> Result<(), String> {
        let c = self.shared.compiled.read().unwrap().clone();
        let cp = c
            .points
            .iter()
            .find(|p| p.point.id == id)
            .ok_or_else(|| format!("点位不存在或地址无效: {id}"))?;
        if cp.point.access != crate::model::Access::ReadWrite || !cp.loc.writable {
            return Err(format!("点位「{}」不可写", cp.point.name));
        }
        let shown = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let result = match cp.encode(value) {
            Err(e) => Err(e),
            Ok(v) => {
                let mut guard = self.shared.driver.lock().await;
                match guard.as_mut() {
                    None => Err("PLC 未连接".to_string()),
                    Some(d) => d.write(&cp.loc, &v).await.map_err(|e| e.to_string()),
                }
            }
        };
        let old = self
            .shared
            .values
            .lock()
            .unwrap()
            .get(id)
            .and_then(|v| v.value.as_ref().map(|v| v.display()));
        let entry = match &result {
            Ok(()) => LogEntry::new(Level::Info, Category::Write, format!("写入 {shown}")),
            Err(e) => LogEntry::new(
                Level::Error,
                Category::Write,
                format!("写入 {shown} 失败：{e}"),
            ),
        };
        self.shared.log(
            entry
                .point(&cp.point.id, &cp.point.name)
                .values(old, Some(shown)),
        );
        result
    }
}
