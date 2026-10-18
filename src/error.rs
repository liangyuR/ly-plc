use std::fmt;

use serde::{Deserialize, Serialize};

use crate::protocol::DriverError;

/// 引擎对外的错误。调用方按变体区分「链路坏了」与「PLC 拒绝了这次请求」，不必匹配文本；
/// 文本只用于展示，措辞可能调整。
///
/// 序列化为 `{"kind": "timeout", "message": "通讯超时"}`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
#[non_exhaustive]
pub enum PlcError {
    /// 没有可用连接：引擎未启动、正在重连，或连接已中断（轮询缓存里的点位被标记离线时也是它）。
    Offline(String),
    /// 通讯错误：连接被重置、应答帧异常、地址无法解析等。连接会被重建。
    Link(String),
    /// 通讯超时（含连接超时）。连接会被重建。
    Timeout(String),
    /// PLC 应答了但拒绝了请求：S7 访问失败 / 请求被拒、COTP 拒绝、Modbus 异常码、MC 异常结束码。
    /// S7 单个点位超出协商 PDU 的单帧容量时不发请求，同样归为此类。
    Rejected(String),
    /// 调用方的问题：点位不存在、不可写、值无法按点位类型编码。
    Invalid(String),
    /// 读到了数据但无法按点位类型解码（STRING 头与点位不符、长度不足）。
    Decode(String),
    /// 写后读回与写入值不一致，且已用尽重试次数（[`crate::PlcEngine::write_point_verified`]）。
    Mismatch(String),
}

impl PlcError {
    pub fn message(&self) -> &str {
        match self {
            PlcError::Offline(m)
            | PlcError::Link(m)
            | PlcError::Timeout(m)
            | PlcError::Rejected(m)
            | PlcError::Invalid(m)
            | PlcError::Decode(m)
            | PlcError::Mismatch(m) => m,
        }
    }

    /// 变体名，与序列化里的 `kind` 一致。
    pub fn kind(&self) -> &'static str {
        match self {
            PlcError::Offline(_) => "offline",
            PlcError::Link(_) => "link",
            PlcError::Timeout(_) => "timeout",
            PlcError::Rejected(_) => "rejected",
            PlcError::Invalid(_) => "invalid",
            PlcError::Decode(_) => "decode",
            PlcError::Mismatch(_) => "mismatch",
        }
    }

    /// 链路类错误（`Offline` / `Link` / `Timeout`）：问题出在连接上，不在这个点位或这个值上。
    pub fn is_link(&self) -> bool {
        matches!(
            self,
            PlcError::Offline(_) | PlcError::Link(_) | PlcError::Timeout(_)
        )
    }

    pub(crate) fn offline() -> Self {
        PlcError::Offline("PLC 未连接".into())
    }
}

impl fmt::Display for PlcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PlcError {}

/// 让返回 `Result<_, String>` 的旧调用方可以继续用 `?`。
impl From<PlcError> for String {
    fn from(e: PlcError) -> Self {
        e.message().to_owned()
    }
}

impl From<DriverError> for PlcError {
    fn from(e: DriverError) -> Self {
        match e {
            DriverError::Link(m) => PlcError::Link(m),
            DriverError::Timeout(m) => PlcError::Timeout(m),
            DriverError::Exception(m) => PlcError::Rejected(m),
        }
    }
}
