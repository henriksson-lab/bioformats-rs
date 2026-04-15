/// Rust reader timing harness — called by bench/run.sh
///
/// Usage: bench_rust <path> <warmup_rounds> <measure_rounds>
/// Prints one line: average nanoseconds per iteration

use std::path::Path;
use std::time::Instant;

use bioformats::FormatReader;
use bioformats::ImageReader;

fn read_all(path: &Path) -> usize {
    let mut reader = ImageReader::open(path).unwrap();
    let count = reader.metadata().image_count;
    let mut total = 0usize;
    for i in 0..count {
        let plane = reader.open_bytes(i).unwrap();
        total += plane.len();
    }
    total
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path          = Path::new(&args[1]);
    let warmup:  u32  = args[2].parse().unwrap();
    let measure: u32  = args[3].parse().unwrap();

    for _ in 0..warmup {
        std::hint::black_box(read_all(path));
    }

    let mut total_ns: u64 = 0;
    for _ in 0..measure {
        let t0 = Instant::now();
        std::hint::black_box(read_all(path));
        total_ns += t0.elapsed().as_nanos() as u64;
    }

    println!("{}", total_ns / measure as u64);
}
