use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;

use super::modbus::{COIL, DISCRETE_INPUT, HOLDING_REGISTER, INPUT_REGISTER};
use super::{
    bytes_to_words, words_to_bytes, DriverError, Location, Protocol, Region, RegionData, Unit,
    WriteValue,
};

pub struct SimMemory {
    started: Instant,
    bits: HashMap<(u16, u32), bool>,
    words: HashMap<(u16, u32), u16>,
}

impl Default for SimMemory {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            bits: HashMap::new(),
            words: HashMap::new(),
        }
    }
}

pub struct Simulator(Arc<Mutex<SimMemory>>);

impl Simulator {
    pub fn new(memory: Arc<Mutex<SimMemory>>) -> Self {
        Self(memory)
    }
}

#[async_trait]
impl Protocol for Simulator {
    async fn read(&mut self, region: &Region) -> Result<RegionData, DriverError> {
        let m = self.0.lock().unwrap();
        let code = region.area.code;
        let range = region.start..region.start + region.count;
        Ok(if region.area.unit == Unit::Bit {
            RegionData::Bits(
                range
                    .map(|a| m.bits.get(&(code, a)).copied().unwrap_or(false))
                    .collect(),
            )
        } else {
            let words: Vec<u16> = range
                .map(|a| m.words.get(&(code, a)).copied().unwrap_or(0))
                .collect();
            RegionData::Bytes(words_to_bytes(&words))
        })
    }

    async fn write(&mut self, loc: &Location, value: &WriteValue) -> Result<(), DriverError> {
        let mut m = self.0.lock().unwrap();
        let code = loc.area.code;
        match (code, value) {
            (COIL, WriteValue::Bit(b)) => {
                m.bits.insert((code, loc.start), *b);
            }
            (HOLDING_REGISTER, WriteValue::Bit(b)) => {
                let bit = loc.bit.unwrap_or(0);
                let w = m.words.entry((code, loc.start)).or_insert(0);
                *w = if *b {
                    *w | (1 << bit)
                } else {
                    *w & !(1 << bit)
                };
            }
            (HOLDING_REGISTER, WriteValue::Bytes(bytes)) => {
                for (i, w) in bytes_to_words(bytes).into_iter().enumerate() {
                    m.words.insert((code, loc.start + i as u32), w);
                }
            }
            _ => return Err(DriverError::Exception("该区域不可写".into())),
        }
        Ok(())
    }

    fn max_region(&self, unit: Unit) -> (u32, u32) {
        if unit == Unit::Bit {
            (1000, 64)
        } else {
            (120, 16)
        }
    }

    fn tick(&mut self) {
        let mut m = self.0.lock().unwrap();
        let secs = m.started.elapsed().as_secs();
        m.words.insert((INPUT_REGISTER, 0), (secs & 0xFFFF) as u16);
        m.bits.insert((DISCRETE_INPUT, 0), (secs / 5) % 2 == 1);
    }
}
