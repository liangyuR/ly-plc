use crate::protocol::{Location, Region, Unit};

pub struct Block {
    pub region: Region,
    pub members: Vec<usize>,
}

pub fn plan(locations: &[&Location], limits: impl Fn(Unit) -> (u32, u32)) -> Vec<Block> {
    let mut order: Vec<usize> = (0..locations.len()).collect();
    order.sort_by_key(|&i| (locations[i].area, locations[i].start));

    let mut blocks: Vec<Block> = Vec::new();
    for i in order {
        let loc = locations[i];
        let end = loc.start + loc.len;
        let (max_len, max_gap) = limits(loc.area.unit);
        if let Some(b) = blocks.last_mut() {
            let r = &mut b.region;
            let r_end = r.start + r.count;
            let new_end = end.max(r_end);
            if r.area == loc.area && loc.start <= r_end + max_gap && new_end - r.start <= max_len {
                r.count = new_end - r.start;
                b.members.push(i);
                continue;
            }
        }
        blocks.push(Block {
            region: Region {
                area: loc.area,
                start: loc.start,
                count: loc.len,
            },
            members: vec![i],
        });
    }
    blocks
}
