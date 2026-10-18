use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{DataType, WordOrder};
use crate::protocol::Unit;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlcValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// `Bytes` 点位的原始字节，JSON 中为数字数组。
    Bytes(Vec<u8>),
}

impl PlcValue {
    pub fn is_truthy(&self) -> bool {
        match self {
            PlcValue::Bool(b) => *b,
            PlcValue::Int(i) => *i != 0,
            PlcValue::Float(f) => *f != 0.0,
            PlcValue::Text(s) => !s.is_empty(),
            PlcValue::Bytes(b) => b.iter().any(|&x| x != 0),
        }
    }

    pub fn display(&self) -> String {
        match self {
            PlcValue::Bool(b) => b.to_string(),
            PlcValue::Int(i) => i.to_string(),
            PlcValue::Float(f) => f.to_string(),
            PlcValue::Text(s) => s.clone(),
            PlcValue::Bytes(b) => b
                .iter()
                .map(|x| format!("{x:02X}"))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// S7 STRING：首字节为声明的最大长度，次字节为实际长度，随后是单字节字符；`bytes` 为点位完整区域。
pub fn decode_string(bytes: &[u8]) -> Result<PlcValue, String> {
    let max = bytes.len().checked_sub(2).ok_or("数据长度不足")?;
    let (declared, actual) = (bytes[0] as usize, bytes[1] as usize);
    if declared != max {
        return Err(format!(
            "PLC 字符串声明长度 {declared} 与点位最大长度 {max} 不符"
        ));
    }
    if actual > max {
        return Err(format!("PLC 字符串实际长度 {actual} 超过最大长度 {max}"));
    }
    Ok(PlcValue::Text(
        bytes[2..2 + actual].iter().map(|&b| b as char).collect(),
    ))
}

pub fn encode_string(value: &Value, max: usize) -> Result<Vec<u8>, String> {
    let chars = single_byte_chars(value, max)?;
    let mut out = vec![0u8; max + 2];
    out[0] = max as u8;
    out[1] = chars.len() as u8;
    out[2..2 + chars.len()].copy_from_slice(&chars);
    Ok(out)
}

/// S7-200 SMART 字符串：首字节为实际长度，随后是单字节字符，没有声明长度字节；`bytes` 为点位完整区域。
pub fn decode_smart_string(bytes: &[u8]) -> Result<PlcValue, String> {
    let max = bytes.len().checked_sub(1).ok_or("数据长度不足")?;
    let actual = bytes[0] as usize;
    if actual > max {
        return Err(format!("PLC 字符串实际长度 {actual} 超过最大长度 {max}"));
    }
    Ok(PlcValue::Text(
        bytes[1..1 + actual].iter().map(|&b| b as char).collect(),
    ))
}

pub fn encode_smart_string(value: &Value, max: usize) -> Result<Vec<u8>, String> {
    let chars = single_byte_chars(value, max)?;
    let mut out = vec![0u8; max + 1];
    out[0] = chars.len() as u8;
    out[1..1 + chars.len()].copy_from_slice(&chars);
    Ok(out)
}

fn single_byte_chars(value: &Value, max: usize) -> Result<Vec<u8>, String> {
    let Value::String(s) = value else {
        return Err("String 类型需写入字符串".into());
    };
    let chars = s
        .chars()
        .map(|c| u8::try_from(c as u32).map_err(|_| format!("字符「{c}」超出 S7 单字节字符范围")))
        .collect::<Result<Vec<u8>, String>>()?;
    if chars.len() > max {
        return Err(format!("字符串长度 {} 超过最大长度 {max}", chars.len()));
    }
    Ok(chars)
}

pub fn encode_bytes(value: &Value, len: usize) -> Result<Vec<u8>, String> {
    let Value::Array(items) = value else {
        return Err("Bytes 类型需写入字节数组，例如 [83, 78, 0]".into());
    };
    if items.len() != len {
        return Err(format!("字节数 {} 与点位长度 {len} 不符", items.len()));
    }
    items
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| format!("无效的字节: {v}"))
        })
        .collect()
}

pub fn reorder(bytes: &[u8], order: WordOrder) -> Vec<u8> {
    let mut b = bytes.to_vec();
    if b.len() < 2 {
        return b;
    }
    if matches!(order, WordOrder::Cdab | WordOrder::Dcba) {
        b = b.chunks(2).rev().flatten().copied().collect();
    }
    if matches!(order, WordOrder::Badc | WordOrder::Dcba) {
        for pair in b.chunks_mut(2) {
            if pair.len() == 2 {
                pair.swap(0, 1);
            }
        }
    }
    b
}

pub fn decode_bit(bytes: &[u8], unit: Unit, bit: u8) -> bool {
    match unit {
        Unit::Word => (u16::from_be_bytes([bytes[0], bytes[1]]) >> bit) & 1 == 1,
        _ => (bytes[0] >> bit) & 1 == 1,
    }
}

pub fn decode(bytes: &[u8], data_type: DataType, order: WordOrder) -> Result<PlcValue, String> {
    let n = data_type.byte_size() as usize;
    if bytes.len() < n || n == 0 {
        return Err("数据长度不足".into());
    }
    let v = reorder(&bytes[..n], order);
    Ok(match data_type {
        DataType::Bool | DataType::String | DataType::SmartString | DataType::Bytes => {
            unreachable!()
        }
        DataType::U8 => PlcValue::Int(v[0] as i64),
        DataType::I8 => PlcValue::Int(v[0] as i8 as i64),
        DataType::U16 => PlcValue::Int(u16::from_be_bytes([v[0], v[1]]) as i64),
        DataType::I16 => PlcValue::Int(i16::from_be_bytes([v[0], v[1]]) as i64),
        DataType::U32 => PlcValue::Int(u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as i64),
        DataType::I32 => PlcValue::Int(i32::from_be_bytes([v[0], v[1], v[2], v[3]]) as i64),
        DataType::F32 => {
            let f = f32::from_be_bytes([v[0], v[1], v[2], v[3]]);
            PlcValue::Float(f.to_string().parse().unwrap_or(f as f64))
        }
        DataType::F64 => PlcValue::Float(f64::from_be_bytes(v[..8].try_into().unwrap())),
    })
}

pub fn parse_bool(value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0) != 0.0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" => Ok(true),
            "0" | "false" | "off" => Ok(false),
            _ => Err(format!("无效的布尔值: {s}")),
        },
        _ => Err("无效的布尔值".into()),
    }
}

fn parse_number(value: &Value) -> Result<f64, String> {
    match value {
        Value::Number(n) => n.as_f64().ok_or_else(|| "无效的数值".into()),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse().map_err(|_| format!("无效的数值: {s}")),
        _ => Err("无效的数值".into()),
    }
}

fn parse_int(value: &Value, min: f64, max: f64) -> Result<i64, String> {
    let v = parse_number(value)?;
    if v.fract() != 0.0 {
        return Err(format!("需要整数: {v}"));
    }
    if v < min || v > max {
        return Err(format!("数值超出范围 [{min}, {max}]: {v}"));
    }
    Ok(v as i64)
}

pub fn encode(value: &Value, data_type: DataType, order: WordOrder) -> Result<Vec<u8>, String> {
    let be: Vec<u8> = match data_type {
        DataType::Bool => vec![parse_bool(value)? as u8],
        DataType::U8 => vec![parse_int(value, 0.0, u8::MAX as f64)? as u8],
        DataType::I8 => vec![parse_int(value, i8::MIN as f64, i8::MAX as f64)? as i8 as u8],
        DataType::U16 => (parse_int(value, 0.0, u16::MAX as f64)? as u16)
            .to_be_bytes()
            .to_vec(),
        DataType::I16 => (parse_int(value, i16::MIN as f64, i16::MAX as f64)? as i16)
            .to_be_bytes()
            .to_vec(),
        DataType::U32 => (parse_int(value, 0.0, u32::MAX as f64)? as u32)
            .to_be_bytes()
            .to_vec(),
        DataType::I32 => (parse_int(value, i32::MIN as f64, i32::MAX as f64)? as i32)
            .to_be_bytes()
            .to_vec(),
        DataType::F32 => (parse_number(value)? as f32).to_be_bytes().to_vec(),
        DataType::F64 => parse_number(value)?.to_be_bytes().to_vec(),
        DataType::String | DataType::SmartString | DataType::Bytes => {
            return Err(format!("{data_type:?} 类型需按点位长度编码"))
        }
    };
    Ok(reorder(&be, order))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_all_orders() {
        for order in [
            WordOrder::Abcd,
            WordOrder::Cdab,
            WordOrder::Badc,
            WordOrder::Dcba,
        ] {
            for (dt, v) in [
                (DataType::I32, json!(-123456)),
                (DataType::F32, json!(1.25)),
                (DataType::F64, json!(-3.5e10)),
                (DataType::I16, json!(-2)),
            ] {
                let bytes = encode(&v, dt, order).unwrap();
                assert_eq!(
                    decode(&bytes, dt, order).unwrap(),
                    serde_json::from_value::<PlcValue>(v).unwrap()
                );
            }
        }
        assert_eq!(
            encode(&json!(0x12345678u32), DataType::U32, WordOrder::Cdab).unwrap(),
            vec![0x56, 0x78, 0x12, 0x34]
        );
    }

    #[test]
    fn s7_string_writes_declared_max_and_round_trips() {
        let bytes = encode_string(&json!("SN-A1"), 8).unwrap();
        assert_eq!(bytes, [8, 5, b'S', b'N', b'-', b'A', b'1', 0, 0, 0]);
        assert_eq!(
            decode_string(&bytes).unwrap(),
            PlcValue::Text("SN-A1".into())
        );
        assert!(encode_string(&json!("123456789"), 8).is_err());
        assert!(encode_string(&json!("编号"), 8).is_err());
    }

    #[test]
    fn smart_string_has_only_an_actual_length_byte_and_round_trips() {
        let bytes = encode_smart_string(&json!("SN-A1"), 8).unwrap();
        assert_eq!(bytes, [5, b'S', b'N', b'-', b'A', b'1', 0, 0, 0]);
        assert_eq!(
            decode_smart_string(&bytes).unwrap(),
            PlcValue::Text("SN-A1".into())
        );
        assert_eq!(
            decode_smart_string(&[0; 9]).unwrap(),
            PlcValue::Text(String::new())
        );
        assert!(decode_smart_string(&[9, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(encode_smart_string(&json!("123456789"), 8).is_err());
        assert!(encode_smart_string(&json!("编号"), 8).is_err());
    }

    #[test]
    fn bytes_round_trip_through_json() {
        let bytes = encode_bytes(&json!([8, 2, 83, 78, 0]), 5).unwrap();
        assert_eq!(bytes, [8, 2, b'S', b'N', 0]);
        let v = PlcValue::Bytes(bytes);
        assert_eq!(v.display(), "08 02 53 4E 00");
        assert!(v.is_truthy());
        assert!(!PlcValue::Bytes(vec![0, 0]).is_truthy());
        let text = serde_json::to_string(&v).unwrap();
        assert_eq!(text, "[8,2,83,78,0]");
        assert_eq!(serde_json::from_str::<PlcValue>(&text).unwrap(), v);
        assert!(encode_bytes(&json!([1, 2]), 3).is_err());
        assert!(encode_bytes(&json!([1, 256, 3]), 3).is_err());
        assert!(encode_bytes(&json!("0102"), 2).is_err());
    }

    #[test]
    fn s7_string_with_mismatched_header_is_rejected() {
        assert!(decode_string(&[0, 0, 0, 0, 0, 0]).is_err());
        assert!(decode_string(&[4, 5, b'a', b'b', b'c', b'd']).is_err());
    }
}
