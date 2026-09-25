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
}

impl PlcValue {
    pub fn is_truthy(&self) -> bool {
        match self {
            PlcValue::Bool(b) => *b,
            PlcValue::Int(i) => *i != 0,
            PlcValue::Float(f) => *f != 0.0,
        }
    }

    pub fn display(&self) -> String {
        match self {
            PlcValue::Bool(b) => b.to_string(),
            PlcValue::Int(i) => i.to_string(),
            PlcValue::Float(f) => f.to_string(),
        }
    }
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
        DataType::Bool => unreachable!(),
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
}
