use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::error::PlcError;
use crate::event::{EdgeEvent, EventSink, PlcEvent};
use crate::logbook::{now_ms, Category, Level, LogEntry, Logbook};
use crate::model::{Access, DataType, EdgeMode, PlcConfig, PlcPoint, WordOrder};
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
    /// `connect()` 以来的拨号总次数（含成功的）。
    pub dials: u64,
    /// 本次掉线以来的拨号次数（含成功的那次）；首次连接时是连上之前的拨号次数。
    /// 链路再次中断时清零，所以恢复之后读到的就是这次恢复用了几次拨号。
    pub outage_dials: u32,
    /// 最近一次拨号失败的原因。连接成功后保留，`connect()` 时清空。
    pub last_dial_error: Option<PlcError>,
    /// 最近一次导致状态变为 `Error` 的错误（拨号失败或已连接后的链路错误）。
    pub last_error: Option<PlcError>,
}

/// 点位在轮询缓存里的值。
///
/// 序列化为 `{"value", "error", "errorKind", "ts"}`：`error` 仍是文本（与 0.1 的 JSON 兼容），
/// `errorKind` 是 [`PlcError::kind`]。
#[derive(Clone, Debug, PartialEq)]
pub struct PointValue {
    pub value: Option<PlcValue>,
    pub error: Option<PlcError>,
    pub ts: i64,
}

impl Serialize for PointValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("PointValue", 4)?;
        s.serialize_field("value", &self.value)?;
        s.serialize_field("error", &self.error.as_ref().map(PlcError::message))?;
        s.serialize_field("errorKind", &self.error.as_ref().map(PlcError::kind))?;
        s.serialize_field("ts", &self.ts)?;
        s.end()
    }
}

/// [`PlcEngine::write_point_verified`] 的参数：最多写几次，每次写后等多久再读回。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifyOptions {
    pub attempts: u32,
    pub delay: Duration,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            delay: Duration::from_millis(5),
        }
    }
}

/// 一次写后确认的过程记录。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VerifiedWrite {
    /// 已经开始的尝试次数（1 起）。
    pub attempts: u32,
    /// 是否至少有一次写入被 PLC 接受。
    pub written: bool,
    /// 最近一次读回的值。
    pub readback: Option<PlcValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyError {
    pub error: PlcError,
    pub report: VerifiedWrite,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for VerifyError {}

impl From<VerifyError> for PlcError {
    fn from(e: VerifyError) -> Self {
        e.error
    }
}

struct CompiledPoint {
    point: PlcPoint,
    loc: Location,
    order: WordOrder,
}

impl CompiledPoint {
    fn encode(&self, value: &Value) -> Result<WriteValue, PlcError> {
        let len = self.loc.len as usize;
        match self.point.data_type {
            DataType::Bool => value::parse_bool(value).map(WriteValue::Bit),
            DataType::String => value::encode_string(value, len - 2).map(WriteValue::Bytes),
            DataType::Bytes => value::encode_bytes(value, len).map(WriteValue::Bytes),
            dt => value::encode(value, dt, self.order).map(WriteValue::Bytes),
        }
        .map_err(PlcError::Invalid)
    }

    /// 只覆盖本点位的读取区域。
    fn region(&self) -> Region {
        Region {
            area: self.loc.area,
            start: self.loc.start,
            count: self.loc.len,
        }
    }

    fn decode(&self, region: &Region, data: &RegionData) -> Result<PlcValue, PlcError> {
        let short = || PlcError::Decode("数据长度不足".into());
        let offset = (self.loc.start - region.start) as usize;
        match data {
            RegionData::Bits(bits) => bits
                .get(offset)
                .map(|b| PlcValue::Bool(*b))
                .ok_or_else(short),
            RegionData::Bytes(bytes) => {
                let ub = self.loc.area.unit.bytes() as usize;
                let start = offset * ub;
                let slice = bytes
                    .get(start..start + self.loc.len as usize * ub)
                    .ok_or_else(short)?;
                match (self.loc.bit, self.point.data_type) {
                    (Some(bit), _) => Ok(PlcValue::Bool(value::decode_bit(
                        slice,
                        self.loc.area.unit,
                        bit,
                    ))),
                    (None, DataType::String) => value::decode_string(slice),
                    (None, DataType::Bytes) => Ok(PlcValue::Bytes(slice.to_vec())),
                    (None, dt) => value::decode(slice, dt, self.order),
                }
                .map_err(PlcError::Decode)
            }
        }
    }

    /// 写入 `written` 之后读回应得到的值（按读取的解码路径算，浮点舍入与读取一致）。
    fn expected(&self, written: &WriteValue) -> Result<PlcValue, PlcError> {
        match written {
            WriteValue::Bit(b) => Ok(PlcValue::Bool(*b)),
            WriteValue::Bytes(bytes) => {
                self.decode(&self.region(), &RegionData::Bytes(bytes.clone()))
            }
        }
    }
}

struct Compiled {
    config: PlcConfig,
    points: Vec<CompiledPoint>,
    heartbeat: Option<(usize, Duration)>,
}

impl Compiled {
    fn find(&self, id: &str) -> Result<&CompiledPoint, PlcError> {
        self.points
            .iter()
            .find(|p| p.point.id == id)
            .ok_or_else(|| PlcError::Invalid(format!("点位不存在或地址无效: {id}")))
    }
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

type Driver = Option<Box<dyn Protocol>>;

/// 链路失败发生在哪个阶段：拨号失败记进 `last_dial_error`；已连接后的失败开始一次新的掉线。
#[derive(Clone, Copy, PartialEq)]
enum Failure {
    Dial,
    Link,
}

struct Shared {
    compiled: RwLock<Arc<Compiled>>,
    status: Mutex<PlcStatus>,
    values: Mutex<HashMap<String, PointValue>>,
    driver: tokio::sync::Mutex<Driver>,
    /// 轮询之外（`read_now` / 写入）遇到的链路错误。连接已在出错时关掉，由轮询任务按掉线处理。
    link_fault: Mutex<Option<PlcError>>,
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

    fn fail(&self, last_error: &mut Option<String>, error: PlcError, stage: Failure) {
        let message = error.to_string();
        self.update_status(|s| {
            if s.state != LinkState::Error {
                s.since = now_ms();
            }
            s.state = LinkState::Error;
            s.message = message.clone();
            s.error_count += 1;
            match stage {
                Failure::Dial => s.last_dial_error = Some(error.clone()),
                Failure::Link => s.outage_dials = 0,
            }
            s.last_error = Some(error);
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

    /// 轮询之外的驱动错误：链路类的立即关掉连接（超时后迟到的应答会错位到下一次请求上），
    /// 留给轮询任务按掉线处理。
    async fn driver_failed(&self, driver: &mut Driver, e: DriverError) -> PlcError {
        let error = PlcError::from(e);
        if error.is_link() {
            if let Some(mut d) = driver.take() {
                d.close().await;
            }
            *self.link_fault.lock().unwrap() = Some(error.clone());
        }
        error
    }

    fn take_fault_or_offline(&self) -> PlcError {
        self.link_fault
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(PlcError::offline)
    }

    async fn read_point(&self, cp: &CompiledPoint) -> Result<PlcValue, PlcError> {
        let region = cp.region();
        let data = {
            let mut guard = self.driver.lock().await;
            let Some(driver) = guard.as_mut() else {
                return Err(PlcError::offline());
            };
            match driver.read(&region).await {
                Ok(data) => data,
                Err(e) => return Err(self.driver_failed(&mut guard, e).await),
            }
        };
        cp.decode(&region, &data)
    }

    /// 写一个点位并记日志，成功时返回实际写下的编码值。
    async fn write_point(&self, cp: &CompiledPoint, value: &Value) -> Result<WriteValue, PlcError> {
        let result = self.write_encoded(cp, value).await;
        let shown = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let old = self
            .values
            .lock()
            .unwrap()
            .get(&cp.point.id)
            .and_then(|v| v.value.as_ref().map(|v| v.display()));
        let entry = match &result {
            Ok(_) => LogEntry::new(Level::Info, Category::Write, format!("写入 {shown}")),
            Err(e) => LogEntry::new(
                Level::Error,
                Category::Write,
                format!("写入 {shown} 失败：{e}"),
            ),
        };
        self.log(
            entry
                .point(&cp.point.id, &cp.point.name)
                .values(old, Some(shown)),
        );
        result
    }

    async fn write_encoded(
        &self,
        cp: &CompiledPoint,
        value: &Value,
    ) -> Result<WriteValue, PlcError> {
        if cp.point.access != Access::ReadWrite || !cp.loc.writable {
            return Err(PlcError::Invalid(format!(
                "点位「{}」不可写",
                cp.point.name
            )));
        }
        let encoded = cp.encode(value)?;
        let mut guard = self.driver.lock().await;
        let Some(driver) = guard.as_mut() else {
            return Err(PlcError::offline());
        };
        match driver.write(&cp.loc, &encoded).await {
            Ok(()) => Ok(encoded),
            Err(e) => Err(self.driver_failed(&mut guard, e).await),
        }
    }

    async fn poll_once(
        &self,
        c: &Compiled,
        heartbeat: &mut (Instant, bool),
    ) -> Result<(), PlcError> {
        let mut guard = self.driver.lock().await;
        let Some(driver) = guard.as_mut() else {
            return Err(self.take_fault_or_offline());
        };
        driver.tick();
        let polled: Vec<usize> = (0..c.points.len())
            .filter(|&i| c.points[i].point.poll)
            .collect();
        let locations: Vec<&Location> = polled.iter().map(|&i| &c.points[i].loc).collect();
        let blocks = plan::plan(&locations, |u| driver.max_region(u));
        let now = now_ms();
        let mut fresh: HashMap<String, PointValue> = HashMap::with_capacity(polled.len());

        for block in &blocks {
            let data = match driver.read(&block.region).await {
                Ok(d) => Ok(d),
                Err(e) if e.is_link() => return Err(e.into()),
                Err(e) => Err(PlcError::from(e)),
            };
            for &m in &block.members {
                let cp = &c.points[polled[m]];
                let decoded = match &data {
                    Ok(d) => cp.decode(&block.region, d),
                    Err(e) => Err(e.clone()),
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
                    match driver.write(&cp.loc, &v).await {
                        Err(e) if e.is_link() => return Err(e.into()),
                        _ => {}
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
            values.retain(|id, _| c.points.iter().any(|p| &p.point.id == id && p.point.poll));
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
                    error: Some(PlcError::Offline(reason.into())),
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

    async fn lose_link(&self, last_error: &mut Option<String>, error: PlcError) {
        self.drop_driver().await;
        self.link_fault.lock().unwrap().take();
        self.mark_offline("通讯中断");
        self.fail(last_error, error, Failure::Link);
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
        let fault = shared.link_fault.lock().unwrap().take();
        if let Some(e) = fault {
            shared.lose_link(&mut last_error, e).await;
            sleep(reconnect).await;
            continue;
        }
        {
            let mut guard = shared.driver.lock().await;
            if guard.is_none() {
                let target = conn.target();
                shared.update_status(|s| {
                    if s.state != LinkState::Connecting {
                        s.since = now_ms();
                    }
                    s.state = LinkState::Connecting;
                    s.message = format!("正在连接 {target}");
                    s.dials += 1;
                    s.outage_dials += 1;
                });
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
                        shared.fail(&mut last_error, e.into(), Failure::Dial);
                        sleep(reconnect).await;
                        continue;
                    }
                }
            }
        }

        let started = Instant::now();
        if let Err(e) = shared.poll_once(&c, &mut heartbeat).await {
            shared.lose_link(&mut last_error, e).await;
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
                dials: 0,
                outage_dials: 0,
                last_dial_error: None,
                last_error: None,
            }),
            values: Mutex::new(HashMap::new()),
            driver: tokio::sync::Mutex::new(None),
            link_fault: Mutex::new(None),
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

    /// 轮询缓存。`poll: false` 的点位不在其中。
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
            self.shared.status.lock().unwrap().outage_dials = 0;
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
        self.shared.link_fault.lock().unwrap().take();
        self.shared.update_status(|s| {
            s.poll_count = 0;
            s.error_count = 0;
            s.dials = 0;
            s.outage_dials = 0;
            s.last_dial_error = None;
            s.last_error = None;
        });
        *task = Some(tokio::spawn(poll_loop(self.shared.clone())));
    }

    pub async fn disconnect(&self) {
        let handle = self.task.lock().unwrap().take();
        let Some(handle) = handle else { return };
        handle.abort();
        self.shared.drop_driver().await;
        self.shared.link_fault.lock().unwrap().take();
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

    /// 立即写一个点位。与轮询共用驱动锁，写入落在两次轮询之间。
    ///
    /// 链路类错误（[`PlcError::is_link`]）会关掉当前连接，由轮询任务按掉线处理并重连。
    pub async fn write_point(&self, id: &str, value: &Value) -> Result<(), PlcError> {
        let c = self.shared.compiled.read().unwrap().clone();
        let cp = c.find(id)?;
        self.shared.write_point(cp, value).await.map(|_| ())
    }

    /// 立即从 PLC 读一个点位：只读这个点位的区域，不经过也不更新轮询缓存，不记日志。
    ///
    /// 与轮询共用驱动锁：轮询正在进行时等它读完这一轮。`poll: false` 的点位也可以读。
    /// 链路类错误的处理同 [`write_point`](Self::write_point)。
    pub async fn read_now(&self, id: &str) -> Result<PlcValue, PlcError> {
        let c = self.shared.compiled.read().unwrap().clone();
        let cp = c.find(id)?;
        self.shared.read_point(cp).await
    }

    /// 写入并读回确认：写 → 等 `delay` → [`read_now`](Self::read_now) → 比较，不一致就重写，最多 `attempts` 次。
    ///
    /// 期望值按读取的解码路径从写下的编码算出（`f32` 的舍入与读取一致）。写入或读回本身失败时立即返回该错误、
    /// 不再重试；用尽次数仍不一致返回 [`PlcError::Mismatch`]。两种情况的 `report` 里都有尝试次数与最后一次读回的值。
    /// 每次写入照常记日志，每次不一致记一条警告。
    pub async fn write_point_verified(
        &self,
        id: &str,
        value: &Value,
        options: VerifyOptions,
    ) -> Result<VerifiedWrite, VerifyError> {
        let mut report = VerifiedWrite::default();
        let c = self.shared.compiled.read().unwrap().clone();
        let fail = |error: PlcError, report: &VerifiedWrite| VerifyError {
            error,
            report: report.clone(),
        };
        let cp = c.find(id).map_err(|e| fail(e, &report))?;
        let attempts = options.attempts.max(1);
        let mut expected = None;
        for attempt in 1..=attempts {
            report.attempts = attempt;
            let written = self
                .shared
                .write_point(cp, value)
                .await
                .map_err(|e| fail(e, &report))?;
            report.written = true;
            if expected.is_none() {
                expected = Some(cp.expected(&written).map_err(|e| fail(e, &report))?);
            }
            if !options.delay.is_zero() {
                sleep(options.delay).await;
            }
            let actual = self
                .shared
                .read_point(cp)
                .await
                .map_err(|e| fail(e, &report))?;
            let matched = Some(&actual) == expected.as_ref();
            report.readback = Some(actual);
            if matched {
                return Ok(report);
            }
            let shown = |v: &Option<PlcValue>| v.as_ref().map(|v| v.display()).unwrap_or_default();
            self.shared.log(
                LogEntry::new(
                    Level::Warn,
                    Category::Write,
                    format!(
                        "读回 {} 与写入 {} 不一致（第 {attempt}/{attempts} 次）",
                        shown(&report.readback),
                        shown(&expected)
                    ),
                )
                .point(&cp.point.id, &cp.point.name),
            );
        }
        let error = PlcError::Mismatch(format!(
            "点位「{}」写后读回不一致，已尝试 {attempts} 次",
            cp.point.name
        ));
        Err(fail(error, &report))
    }
}
