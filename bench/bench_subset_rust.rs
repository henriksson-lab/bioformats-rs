/// Rust subset-read timing harness for bench/compare_subset.sh.
///
/// Usage: bench_subset_rust <path> <warmup_rounds> <measure_rounds>
///                          <planes_per_series> <max_region_width>
///                          <max_region_height>
///
/// Prints key=value lines. Timing excludes process startup and includes
/// ImageReader::open + centered region reads for each measured iteration.

use std::path::Path;
use std::time::Instant;

use bioformats::ImageReader;

#[derive(Default)]
struct ReadStats {
    bytes: usize,
    series: usize,
    planes: usize,
    max_width: u32,
    max_height: u32,
}

fn read_subset(
    path: &Path,
    planes_per_series: u32,
    max_region_width: u32,
    max_region_height: u32,
) -> bioformats::Result<ReadStats> {
    let mut reader = ImageReader::open(path)?;
    let mut stats = ReadStats::default();
    let series_count = reader.series_count();

    for series in 0..series_count {
        reader.set_series(series)?;
        let meta = reader.metadata();
        let image_count = meta.image_count;
        let size_x = meta.size_x;
        let size_y = meta.size_y;
        let w = size_x.min(max_region_width).max(1);
        let h = size_y.min(max_region_height).max(1);
        let x = (size_x - w) / 2;
        let y = (size_y - h) / 2;
        let planes = image_count.min(planes_per_series);

        stats.series += 1;
        stats.max_width = stats.max_width.max(size_x);
        stats.max_height = stats.max_height.max(size_y);

        for plane in 0..planes {
            let bytes = reader.open_bytes_region(plane, x, y, w, h)?;
            stats.bytes += bytes.len();
            stats.planes += 1;
        }
    }

    reader.close()?;
    Ok(stats)
}

fn print_error(message: impl std::fmt::Display) {
    println!("status=error");
    println!("error={}", one_line(message.to_string()));
}

fn one_line(mut s: String) -> String {
    s.retain(|c| c != '\n' && c != '\r');
    s
}

fn parse_arg<T: std::str::FromStr>(args: &[String], index: usize, name: &str) -> T {
    args.get(index)
        .unwrap_or_else(|| panic!("missing {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("invalid {name}"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        eprintln!(
            "usage: bench_subset_rust <path> <warmup> <measure> <planes_per_series> <max_w> <max_h>"
        );
        std::process::exit(2);
    }

    let path = Path::new(&args[1]);
    let warmup: u32 = parse_arg(&args, 2, "warmup");
    let measure: u32 = parse_arg(&args, 3, "measure");
    let planes_per_series: u32 = parse_arg(&args, 4, "planes_per_series");
    let max_region_width: u32 = parse_arg(&args, 5, "max_region_width");
    let max_region_height: u32 = parse_arg(&args, 6, "max_region_height");

    if measure == 0 || planes_per_series == 0 || max_region_width == 0 || max_region_height == 0 {
        print_error("measure, planes_per_series, max_w, and max_h must be positive");
        std::process::exit(1);
    }

    for _ in 0..warmup {
        if let Err(err) = read_subset(path, planes_per_series, max_region_width, max_region_height)
        {
            print_error(err);
            std::process::exit(1);
        }
    }

    let mut total_ns: u128 = 0;
    let mut last_stats = ReadStats::default();
    for _ in 0..measure {
        let t0 = Instant::now();
        match read_subset(path, planes_per_series, max_region_width, max_region_height) {
            Ok(stats) => {
                std::hint::black_box(&stats);
                last_stats = stats;
            }
            Err(err) => {
                print_error(err);
                std::process::exit(1);
            }
        }
        total_ns += t0.elapsed().as_nanos();
    }

    println!("status=ok");
    println!("avg_ns={}", total_ns / u128::from(measure));
    println!("bytes={}", last_stats.bytes);
    println!("series={}", last_stats.series);
    println!("planes={}", last_stats.planes);
    println!("max_width={}", last_stats.max_width);
    println!("max_height={}", last_stats.max_height);
}
