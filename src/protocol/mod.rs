pub mod mc;
pub mod modbus;
pub mod s7;
pub mod sim;

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::model::{ConnectionConfig, DataType, ProtocolKind, WordOrder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unit {
    Bit,
    Byte,
    Word,
}

impl Unit {
    pub fn bytes(self) -> u32 {
        match self {
            Unit::Bit => 0,
            Unit::Byte => 1,
            Unit::Word => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AreaKey {
    pub code: u16,
    pub index: u16,
    pub unit: Unit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub area: AreaKey,
    pub start: u32,
    pub len: u32,
    pub bit: Option<u8>,
    pub writable: bool,
}

#[derive(Clone, Debug)]
pub struct Region {
    pub area: AreaKey,
    pub start: u32,
    pub count: u32,
}

pub enum RegionData {
    Bits(Vec<bool>),
    Bytes(Vec<u8>),
}

pub enum WriteValue {
    Bit(bool),
    Bytes(Vec<u8>),
}

pub enum DriverError {
    Link(String),
    Exception(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Link(m) | DriverError::Exception(m) => f.write_str(m),
        }
    }
}

#[async_trait]
pub trait Protocol: Send {
    async fn read(&mut self, region: &Region) -> Result<RegionData, DriverError>;
    async fn write(&mut self, loc: &Location, value: &WriteValue) -> Result<(), DriverError>;
    fn max_region(&self, unit: Unit) -> (u32, u32);
    async fn close(&mut self) {}
    fn tick(&mut self) {}
}

pub fn resolve(
    conn: &ConnectionConfig,
    address: &str,
    data_type: DataType,
) -> Result<Location, String> {
    if address.trim().is_empty() {
        return Err("地址不能为空".into());
    }
    match conn.protocol {
        ProtocolKind::ModbusTcp | ProtocolKind::Simulator => modbus::resolve(address, data_type),
        ProtocolKind::S7 => s7::resolve(address, data_type),
        ProtocolKind::Mc => mc::resolve(address, data_type, conn.mc.xy_octal),
    }
}

pub fn describe_address(
    conn: &ConnectionConfig,
    address: &str,
    data_type: DataType,
) -> Result<String, String> {
    let loc = resolve(conn, address, data_type)?;
    let unit = match loc.area.unit {
        Unit::Bit => "位",
        Unit::Byte => "字节",
        Unit::Word => "字",
    };
    let bit = loc.bit.map(|b| format!("，位 {b}")).unwrap_or_default();
    let access = if loc.writable { "可写" } else { "只读" };
    Ok(format!(
        "起始 {}，占 {} {unit}{bit}，{access}",
        loc.start, loc.len
    ))
}

pub fn default_order(kind: ProtocolKind) -> WordOrder {
    match kind {
        ProtocolKind::Mc => WordOrder::Cdab,
        _ => WordOrder::Abcd,
    }
}

pub async fn connect(
    cfg: &ConnectionConfig,
    sim: &Arc<Mutex<sim::SimMemory>>,
) -> Result<Box<dyn Protocol>, String> {
    Ok(match cfg.protocol {
        ProtocolKind::Simulator => Box::new(sim::Simulator::new(sim.clone())),
        ProtocolKind::ModbusTcp => Box::new(modbus::ModbusTcp::connect(cfg).await?),
        ProtocolKind::S7 => Box::new(s7::S7::connect(cfg).await?),
        ProtocolKind::Mc => Box::new(mc::Mc::connect(cfg).await?),
    })
}

pub(crate) fn locate(
    area: AreaKey,
    start: u32,
    bit: Option<u8>,
    data_type: DataType,
    writable: bool,
    max_address: u32,
) -> Result<Location, String> {
    if data_type == DataType::String {
        return Err("String 类型仅支持 S7 字节地址".into());
    }
    let len = match area.unit {
        Unit::Bit => {
            if data_type != DataType::Bool {
                return Err("位软元件/线圈仅支持 Bool 类型".into());
            }
            if bit.is_some() {
                return Err("位地址不能再指定位号".into());
            }
            1
        }
        Unit::Byte | Unit::Word => {
            if data_type == DataType::Bool {
                let b = bit.ok_or("Bool 类型需指定位号，例如 .0")?;
                let max = if area.unit == Unit::Byte { 7 } else { 15 };
                if b > max {
                    return Err(format!("位号需在 0-{max} 之间"));
                }
                1
            } else {
                if bit.is_some() {
                    return Err("仅 Bool 类型可指定位号".into());
                }
                let size = data_type.byte_size();
                if area.unit == Unit::Word && size == 1 {
                    return Err("字寄存器不支持 8 位类型".into());
                }
                size / area.unit.bytes()
            }
        }
    };
    if start as u64 + len as u64 - 1 > max_address as u64 {
        return Err("地址越界".into());
    }
    Ok(Location {
        area,
        start,
        len,
        bit,
        writable,
    })
}

pub(crate) fn split_bit(s: &str, radix: u32) -> Result<(&str, Option<u8>), String> {
    match s.split_once('.') {
        None => Ok((s, None)),
        Some((main, bit)) => {
            let b = u8::from_str_radix(bit, radix).map_err(|_| format!("无效的位号: {bit}"))?;
            Ok((main, Some(b)))
        }
    }
}

pub(crate) fn normalize(address: &str) -> String {
    address
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

pub(crate) fn words_to_bytes(words: &[u16]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_be_bytes()).collect()
}

pub(crate) fn bytes_to_words(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
        .collect()
}

pub(crate) async fn resolve_addr(
    host: &str,
    port: u16,
    dur: Duration,
) -> Result<SocketAddr, String> {
    let target = format!("{}:{}", host.trim(), port);
    let mut addrs = timeout(dur, tokio::net::lookup_host(target.clone()))
        .await
        .map_err(|_| format!("解析地址超时: {target}"))?
        .map_err(|e| format!("无法解析地址 {target}: {e}"))?;
    addrs.next().ok_or_else(|| format!("无法解析地址 {target}"))
}

pub(crate) async fn tcp_connect(host: &str, port: u16, dur: Duration) -> Result<TcpStream, String> {
    let addr = resolve_addr(host, port, dur).await?;
    let stream = timeout(dur, TcpStream::connect(addr))
        .await
        .map_err(|_| format!("连接超时: {addr}"))?
        .map_err(|e| format!("连接失败 {addr}: {e}"))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}
