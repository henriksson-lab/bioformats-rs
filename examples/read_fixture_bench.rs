use bioformats::ImageReader;
use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("usage: read_fixture_bench <path> [planes]");
        std::process::exit(2);
    }

    let path = Path::new(&args[1]);
    let requested_planes = args.get(2).and_then(|s| s.parse::<u32>().ok());

    let open_start = Instant::now();
    let mut reader = ImageReader::open(path).expect("open failed");
    let open_elapsed = open_start.elapsed();

    let metadata = reader.metadata().clone();
    let planes = requested_planes
        .unwrap_or(metadata.image_count)
        .min(metadata.image_count);
    let read_start = Instant::now();
    let mut bytes = 0usize;
    for plane in 0..planes {
        bytes += reader.open_bytes(plane).expect("read failed").len();
    }
    let read_elapsed = read_start.elapsed();

    println!(
        "file={} size={}x{} planes_read={} image_count={} bytes={} open_ms={:.3} read_ms={:.3} total_ms={:.3}",
        path.display(),
        metadata.size_x,
        metadata.size_y,
        planes,
        metadata.image_count,
        bytes,
        open_elapsed.as_secs_f64() * 1000.0,
        read_elapsed.as_secs_f64() * 1000.0,
        (open_elapsed + read_elapsed).as_secs_f64() * 1000.0,
    );
}
