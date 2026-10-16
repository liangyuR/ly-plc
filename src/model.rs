use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::protocol;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtocolKind {
    ModbusTcp,
    S7,
    Mc,
    Simulator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataType {
    Bool,
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
    String,
    /// 原始字节，长度写在地址里（仅 S7，如 `DB1.DBB38.26`），值为 `PlcValue::Bytes`。
    Bytes,
}

impl DataType {
    /// 定长类型的字节数；`Bool` / `String` / `Bytes` 的长度取决于地址，返回 0。
    pub fn byte_size(self) -> u32 {
        match self {
            DataType::Bool | DataType::String | DataType::Bytes => 0,
            DataType::U8 | DataType::I8 => 1,
            DataType::U16 | DataType::I16 => 2,
            DataType::U32 | DataType::I32 | DataType::F32 => 4,
            DataType::F64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WordOrder {
    #[default]
    #[serde(rename = "ABCD")]
    Abcd,
    #[serde(rename = "CDAB")]
    Cdab,
    #[serde(rename = "BADC")]
    Badc,
    #[serde(rename = "DCBA")]
    Dcba,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    #[default]
    Read,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EdgeMode {
    #[default]
    None,
    Rising,
    Falling,
    Both,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlcPoint {
    pub id: String,
    pub name: String,
    pub address: String,
    pub data_type: DataType,
    pub word_order: Option<WordOrder>,
    pub access: Access,
    pub edge: EdgeMode,
    pub log_changes: bool,
    /// 为 `false` 时不参与轮询：不进 `values()`、不产生读取错误日志，只能写或用 `read_now` 按需读。
    pub poll: bool,
    pub tags: Vec<String>,
    pub description: String,
}

impl Default for PlcPoint {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            address: String::new(),
            data_type: DataType::U16,
            word_order: None,
            access: Access::Read,
            edge: EdgeMode::None,
            log_changes: true,
            poll: true,
            tags: Vec::new(),
            description: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModbusOptions {
    pub unit_id: u8,
}

impl Default for ModbusOptions {
    fn default() -> Self {
        Self { unit_id: 1 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum S7ConnectionType {
    #[default]
    Pg,
    Op,
    Basic,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct S7Options {
    pub rack: u8,
    pub slot: u8,
    pub connection_type: S7ConnectionType,
    pub local_tsap: Option<u16>,
    pub remote_tsap: Option<u16>,
    pub pdu_size: u16,
}

impl Default for S7Options {
    fn default() -> Self {
        Self {
            rack: 0,
            slot: 1,
            connection_type: S7ConnectionType::Pg,
            local_tsap: None,
            remote_tsap: None,
            pdu_size: 480,
        }
    }
}

impl S7Options {
    pub fn tsaps(&self) -> (u16, u16) {
        let kind: u16 = match self.connection_type {
            S7ConnectionType::Pg => 1,
            S7ConnectionType::Op => 2,
            S7ConnectionType::Basic => 3,
        };
        let remote = (kind << 8) | ((self.rack as u16 & 0x07) << 5) | (self.slot as u16 & 0x1F);
        (
            self.local_tsap.unwrap_or(0x0100),
            self.remote_tsap.unwrap_or(remote),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McOptions {
    pub network_no: u8,
    pub pc_no: u8,
    pub module_io: u16,
    pub module_station: u8,
    pub xy_octal: bool,
}

impl Default for McOptions {
    fn default() -> Self {
        Self {
            network_no: 0,
            pc_no: 0xFF,
            module_io: 0x03FF,
            module_station: 0,
            xy_octal: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionConfig {
    pub protocol: ProtocolKind,
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub reconnect_interval_ms: u64,
    pub modbus: ModbusOptions,
    pub s7: S7Options,
    pub mc: McOptions,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            protocol: ProtocolKind::Simulator,
            host: "192.168.0.10".into(),
            port: 502,
            timeout_ms: 1000,
            poll_interval_ms: 200,
            reconnect_interval_ms: 3000,
            modbus: ModbusOptions::default(),
            s7: S7Options::default(),
            mc: McOptions::default(),
        }
    }
}

impl ConnectionConfig {
    pub fn target(&self) -> String {
        let host = format!("{}:{}", self.host.trim(), self.port);
        match self.protocol {
            ProtocolKind::Simulator => "模拟器".into(),
            ProtocolKind::ModbusTcp => format!("Modbus TCP {host} (站号 {})", self.modbus.unit_id),
            ProtocolKind::S7 => format!("S7 {host} (机架 {} 插槽 {})", self.s7.rack, self.s7.slot),
            ProtocolKind::Mc => format!("MC 3E {host}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatConfig {
    pub point_id: Option<String>,
    pub interval_ms: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            point_id: None,
            interval_ms: 1000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlcConfig {
    pub connection: ConnectionConfig,
    pub points: Vec<PlcPoint>,
    pub heartbeat: HeartbeatConfig,
    pub log_retention_days: u32,
    pub auto_connect: bool,
}

impl Default for PlcConfig {
    fn default() -> Self {
        Self {
            connection: ConnectionConfig::default(),
            points: Vec::new(),
            heartbeat: HeartbeatConfig::default(),
            log_retention_days: 30,
            auto_connect: false,
        }
    }
}

impl PlcConfig {
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("写入配置失败: {e}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        let c = &self.connection;
        if c.protocol != ProtocolKind::Simulator && c.host.trim().is_empty() {
            return Err("请填写 PLC 地址".into());
        }
        if c.poll_interval_ms < 20 {
            return Err("轮询周期不能小于 20 ms".into());
        }
        if c.timeout_ms < 100 {
            return Err("超时时间不能小于 100 ms".into());
        }
        if c.protocol == ProtocolKind::S7 && !(240..=960).contains(&c.s7.pdu_size) {
            return Err("S7 PDU 长度需在 240-960 之间".into());
        }
        let mut ids = HashSet::new();
        for p in &self.points {
            if p.id.trim().is_empty() {
                return Err("存在缺少 ID 的点位".into());
            }
            if p.name.trim().is_empty() {
                return Err(format!("点位 {} 缺少名称", p.id));
            }
            if !ids.insert(p.id.as_str()) {
                return Err(format!("点位 ID 重复: {}", p.id));
            }
            let loc = protocol::resolve(c, &p.address, p.data_type)
                .map_err(|e| format!("点位「{}」: {e}", p.name))?;
            if p.access == Access::ReadWrite && !loc.writable {
                return Err(format!("点位「{}」所在区域只读", p.name));
            }
            if !p.poll && p.edge != EdgeMode::None {
                return Err(format!("点位「{}」不轮询，不能配置边沿", p.name));
            }
        }
        if let Some(id) = &self.heartbeat.point_id {
            let p = self
                .points
                .iter()
                .find(|p| &p.id == id)
                .ok_or_else(|| "心跳点位不存在".to_string())?;
            if p.access != Access::ReadWrite {
                return Err(format!("心跳点位「{}」需设为读写", p.name));
            }
            if self.heartbeat.interval_ms < 100 {
                return Err("心跳周期不能小于 100 ms".into());
            }
        }
        Ok(())
    }
}
