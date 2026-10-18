//! 进程内的 S7 测试服务端，给集成测试用：按 S7comm 应答 COTP 连接、通讯协商、单项读写，
//! 并能注入 tools/s7_server.py 做不到的情况——记录每一次读请求、拒绝 COTP、只收不回（超时）、
//! 掐断所有连接、写入后被「PLC 程序」改回去。
//!
//! 只实现 ly-plc 驱动会发的报文形状，不是通用的 S7 服务端。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ly_plc::{
    Access, ConnectionConfig, DataType, EventSink, Logbook, PlcConfig, PlcEngine, PlcPoint,
    ProtocolKind,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

pub const AREA_DB: u8 = 0x84;
pub const AREA_M: u8 = 0x83;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// COTP 连接请求回一个非 CC 的应答：拨号失败、错误类型是 `Rejected`。
    RejectCotp,
    /// 收请求但永远不回：客户端超时。
    Stall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    pub area: u8,
    pub db: u16,
    pub start: u32,
    pub count: u32,
}

struct Pin {
    area: u8,
    db: u16,
    offset: usize,
    value: u8,
    remaining: Option<u32>,
}

struct State {
    memory: HashMap<(u8, u16), Vec<u8>>,
    reads: Vec<Read>,
    writes: u32,
    mode: Mode,
    accepted: u32,
    pins: Vec<Pin>,
    pdu: u16,
}

pub struct S7Server {
    pub port: u16,
    state: Arc<Mutex<State>>,
    kill: watch::Sender<u64>,
    task: tokio::task::JoinHandle<()>,
}

impl S7Server {
    /// 起一个服务端，注册 DB1、DB2（各 256 字节）与 M 区。
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut memory = HashMap::new();
        memory.insert((AREA_DB, 1), vec![0u8; 256]);
        memory.insert((AREA_DB, 2), vec![0u8; 256]);
        memory.insert((AREA_M, 0), vec![0u8; 256]);
        let state = Arc::new(Mutex::new(State {
            memory,
            reads: Vec::new(),
            writes: 0,
            mode: Mode::Normal,
            accepted: 0,
            pins: Vec::new(),
            pdu: 480,
        }));
        let (kill, _) = watch::channel(0u64);
        let task = {
            let state = state.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let _ = stream.set_nodelay(true);
                    state.lock().unwrap().accepted += 1;
                    tokio::spawn(serve(stream, state.clone(), kill.subscribe()));
                }
            })
        };
        Self {
            port,
            state,
            kill,
            task,
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
    }

    /// 通讯协商时应答的 PDU（默认 480），不管客户端请求多少；只影响之后建立的连接。
    pub fn set_pdu(&self, pdu: u16) {
        self.state.lock().unwrap().pdu = pdu;
    }

    /// 掐断当前所有连接（新连接照常接受）。
    pub fn kill_connections(&self) {
        self.kill.send_modify(|g| *g += 1);
    }

    pub fn accepted(&self) -> u32 {
        self.state.lock().unwrap().accepted
    }

    pub fn reads(&self) -> Vec<Read> {
        self.state.lock().unwrap().reads.clone()
    }

    pub fn clear_reads(&self) {
        self.state.lock().unwrap().reads.clear();
    }

    pub fn writes(&self) -> u32 {
        self.state.lock().unwrap().writes
    }

    pub fn set_db(&self, db: u16, offset: usize, bytes: &[u8]) {
        let mut s = self.state.lock().unwrap();
        let mem = s.memory.get_mut(&(AREA_DB, db)).unwrap();
        mem[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    pub fn db(&self, db: u16, offset: usize, len: usize) -> Vec<u8> {
        let s = self.state.lock().unwrap();
        s.memory[&(AREA_DB, db)][offset..offset + len].to_vec()
    }

    /// 之后每次写入覆盖 `DB{db}.{offset}` 时，「PLC 程序」随即把它改回 `value`；`times` 为 `None` 表示一直如此。
    pub fn pin_db(&self, db: u16, offset: usize, value: u8, times: Option<u32>) {
        self.state.lock().unwrap().pins.push(Pin {
            area: AREA_DB,
            db,
            offset,
            value,
            remaining: times,
        });
    }
}

impl Drop for S7Server {
    fn drop(&mut self) {
        self.task.abort();
        self.kill_connections();
    }
}

async fn read_frame(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let len = u16::from_be_bytes([head[2], head[3]]) as usize;
    let mut buf = vec![0u8; len];
    buf[..4].copy_from_slice(&head);
    stream.read_exact(&mut buf[4..]).await?;
    Ok(buf)
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>, mut kill: watch::Receiver<u64>) {
    kill.mark_unchanged();
    loop {
        let frame = tokio::select! {
            f = read_frame(&mut stream) => match f {
                Ok(f) => f,
                Err(_) => return,
            },
            _ = kill.changed() => return,
        };
        let mode = state.lock().unwrap().mode;
        if mode == Mode::Stall {
            continue;
        }
        let reply = if frame.get(5) == Some(&0xE0) {
            let mut cc = frame.clone();
            cc[5] = if mode == Mode::RejectCotp { 0x80 } else { 0xD0 };
            cc
        } else {
            let s7 = &frame[7..];
            let body = handle_job(s7, &state);
            let mut out = vec![0x03, 0x00, 0x00, 0x00, 0x02, 0xF0, 0x80];
            out.extend_from_slice(&body);
            let len = out.len() as u16;
            out[2..4].copy_from_slice(&len.to_be_bytes());
            out
        };
        if stream.write_all(&reply).await.is_err() {
            return;
        }
    }
}

fn ack(job: &[u8], params: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x32, 0x03, 0x00, 0x00, job[4], job[5]];
    out.extend_from_slice(&(params.len() as u16).to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(params);
    out.extend_from_slice(data);
    out
}

fn handle_job(s7: &[u8], state: &Mutex<State>) -> Vec<u8> {
    let func = s7[10];
    if func == 0xF0 {
        let [ph, pl] = state.lock().unwrap().pdu.to_be_bytes();
        return ack(s7, &[0xF0, 0x00, 0x00, 0x01, 0x00, 0x01, ph, pl], &[]);
    }
    let item = &s7[12..24];
    let transport = item[3];
    let count = u16::from_be_bytes([item[4], item[5]]) as usize;
    let db = u16::from_be_bytes([item[6], item[7]]);
    let area = item[8];
    let addr = ((item[9] as u32) << 16) | ((item[10] as u32) << 8) | item[11] as u32;
    let mut s = state.lock().unwrap();
    let State {
        memory,
        reads,
        writes,
        pins,
        ..
    } = &mut *s;
    match func {
        0x04 => {
            let start = (addr / 8) as usize;
            reads.push(Read {
                area,
                db,
                start: start as u32,
                count: count as u32,
            });
            let data = match memory.get(&(area, db)) {
                None => vec![0x0A, 0x00, 0x00, 0x00],
                Some(mem) if start + count > mem.len() => vec![0x05, 0x00, 0x00, 0x00],
                Some(mem) => {
                    let mut d = vec![0xFF, 0x04];
                    d.extend_from_slice(&((count * 8) as u16).to_be_bytes());
                    d.extend_from_slice(&mem[start..start + count]);
                    d
                }
            };
            ack(s7, &[0x04, 0x01], &data)
        }
        0x05 => {
            let payload = &s7[28..];
            let code = match memory.get_mut(&(area, db)) {
                None => 0x0A,
                Some(mem) => {
                    let (start, end) = if transport == 0x01 {
                        let (byte, bit) = ((addr / 8) as usize, addr % 8);
                        if byte >= mem.len() {
                            return ack(s7, &[0x05, 0x01], &[0x05]);
                        }
                        if payload[0] != 0 {
                            mem[byte] |= 1 << bit;
                        } else {
                            mem[byte] &= !(1 << bit);
                        }
                        (byte, byte + 1)
                    } else {
                        let start = (addr / 8) as usize;
                        if start + count > mem.len() {
                            return ack(s7, &[0x05, 0x01], &[0x05]);
                        }
                        mem[start..start + count].copy_from_slice(&payload[..count]);
                        (start, start + count)
                    };
                    *writes += 1;
                    for pin in pins.iter_mut() {
                        let hit = pin.area == area
                            && pin.db == db
                            && (start..end).contains(&pin.offset)
                            && pin.remaining != Some(0);
                        if hit {
                            mem[pin.offset] = pin.value;
                            if let Some(n) = &mut pin.remaining {
                                *n -= 1;
                            }
                        }
                    }
                    0xFF
                }
            };
            ack(s7, &[0x05, 0x01], &[code])
        }
        _ => ack(s7, &[func, 0x00], &[]),
    }
}

// ---- 引擎侧的辅助 ----

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct Rig {
    pub engine: PlcEngine,
    pub logbook: Arc<Logbook>,
    db: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db);
    }
}

pub fn point(id: &str, address: &str, data_type: DataType) -> PlcPoint {
    PlcPoint {
        id: id.into(),
        name: id.into(),
        address: address.into(),
        data_type,
        access: Access::ReadWrite,
        ..PlcPoint::default()
    }
}

pub fn config(port: u16, poll_interval_ms: u64, points: Vec<PlcPoint>) -> PlcConfig {
    PlcConfig {
        connection: ConnectionConfig {
            protocol: ProtocolKind::S7,
            host: "127.0.0.1".into(),
            port,
            timeout_ms: 300,
            poll_interval_ms,
            reconnect_interval_ms: 200,
            ..ConnectionConfig::default()
        },
        points,
        ..PlcConfig::default()
    }
}

pub fn rig(config: PlcConfig) -> Rig {
    let db = std::env::temp_dir().join(format!(
        "ly-plc-test-{}-{}.db",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let sink: EventSink = Arc::new(|_| {});
    config.validate().expect("test config must be valid");
    let logbook = Arc::new(Logbook::open(&db, sink.clone(), 1).unwrap());
    let engine = PlcEngine::new(config, logbook.clone(), sink);
    Rig {
        engine,
        logbook,
        db,
    }
}

pub async fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let started = Instant::now();
    while !cond() {
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl Rig {
    /// 连上并等到第一轮轮询完成。
    pub async fn start(&self) {
        self.engine.connect().await;
        wait_until("first poll", Duration::from_secs(3), || {
            self.engine.status().poll_count >= 1
        })
        .await;
    }

    pub async fn wait_polls(&self, n: u64) {
        let target = self.engine.status().poll_count + n;
        wait_until("polls", Duration::from_secs(5), || {
            self.engine.status().poll_count >= target
        })
        .await;
    }
}
