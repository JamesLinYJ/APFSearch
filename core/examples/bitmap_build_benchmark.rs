//! Compare construction from monotonically increasing matched slots in memory.
#[path = "../src/bitmap_builder.rs"]
mod bitmap_builder;
use roaring::RoaringBitmap;
use serde_json::json;
use std::{hint::black_box, time::Instant};

fn build(slots: &[u32], method: usize) -> RoaringBitmap {
    let mut bitmap = RoaringBitmap::new();
    match method {
        0 => {
            for &slot in slots {
                bitmap.insert(slot);
            }
        }
        1 => {
            for &slot in slots {
                bitmap.try_push(slot).unwrap();
            }
        }
        2 => bitmap.extend(slots.iter().copied()),
        3 => {
            let mut builder = bitmap_builder::RunBitmapBuilder::default();
            for &slot in slots {
                builder.insert(slot);
            }
            bitmap = builder.finish();
        }
        _ => unreachable!(),
    }
    bitmap
}

fn main() {
    let mut report = Vec::new();
    for (name, slots) in [
        ("dense", (0..1_000_000u32).collect::<Vec<_>>()),
        ("sparse", (0..1_000_000u32).step_by(97).collect()),
        (
            "clustered",
            (0..1_000_000u32).filter(|slot| slot % 4096 < 768).collect(),
        ),
    ] {
        let expected: RoaringBitmap = slots.iter().copied().collect();
        let mut samples = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for run in 0..30 {
            for offset in 0..4 {
                let method = (run + offset) % 4;
                let started = Instant::now();
                let result = black_box(build(black_box(&slots), method));
                samples[method].push(started.elapsed().as_secs_f64() * 1000.);
                assert_eq!(result, expected);
            }
        }
        for (method, samples) in samples.iter_mut().enumerate() {
            samples.sort_by(f64::total_cmp);
            let method_name = ["insert", "try_push", "extend", "runs"][method];
            report.push(json!({"distribution":name,"matches":slots.len(),"method":method_name,"median_ms":samples[15],"p95_ms":samples[28]}));
        }
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
