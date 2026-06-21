/// Benchmark: read all pixel data from tubhiswt_C0.ome.tif
///
/// Compares:
///   - Rust  : bioformats ImageReader (criterion, many iterations)
///   - Java  : Bio-Formats ImageReader via subprocess (single warm run reported as a custom value)
///
/// Run with:
///   cargo bench -p bioformats --bench read_ome_tiff
///
/// The Java result is printed to stdout; the Rust result appears in the criterion HTML report.
use std::hint::black_box;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

// ── paths ─────────────────────────────────────────────────────────────────────

fn repo_root() -> std::path::PathBuf {
    // bench binary lives in bioformats-rs/target/…; repo root is four levels up
    let mut p = std::env::current_exe().unwrap();
    for _ in 0..4 {
        p.pop();
    } // strip …/target/criterion/<bench>/<bin>
      // Fallback: walk up until we find the marker file
    let mut candidate = std::env::current_dir().unwrap();
    for _ in 0..10 {
        if candidate.join("bioformats_package.jar").exists() {
            return candidate;
        }
        if !candidate.pop() {
            break;
        }
    }
    // last resort: relative to workspace
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap() // crates/bioformats  → crates
        .parent()
        .unwrap() // crates             → bioformats-rs
        .parent()
        .unwrap() // bioformats-rs      → repo root
        .to_path_buf()
}

// ── Java timing helper ────────────────────────────────────────────────────────

/// Small self-contained Java program source that reads every plane and prints
/// elapsed milliseconds to stdout.  Compiled once into JAVA_CLASS_DIR.
const JAVA_SRC: &str = r#"
import loci.formats.ImageReader;
import loci.common.DebugTools;

public class BfBench {
    public static void main(String[] args) throws Exception {
        DebugTools.setRootLevel("ERROR");
        String path = args[0];
        int warmupRounds  = Integer.parseInt(args[1]);
        int measureRounds = Integer.parseInt(args[2]);

        // warm-up (JIT)
        for (int w = 0; w < warmupRounds; w++) {
            ImageReader r = new ImageReader();
            r.setId(path);
            for (int i = 0; i < r.getImageCount(); i++) r.openBytes(i);
            r.close();
        }

        // measure
        long totalNs = 0;
        for (int m = 0; m < measureRounds; m++) {
            ImageReader r = new ImageReader();
            long t0 = System.nanoTime();
            r.setId(path);
            for (int i = 0; i < r.getImageCount(); i++) r.openBytes(i);
            long t1 = System.nanoTime();
            r.close();
            totalNs += (t1 - t0);
        }
        System.out.println(totalNs / measureRounds);   // avg ns per iteration
    }
}
"#;

fn ensure_java_class(root: &Path) -> std::path::PathBuf {
    let class_dir = root.join("target").join("bench-java");
    std::fs::create_dir_all(&class_dir).unwrap();
    let src = class_dir.join("BfBench.java");
    let class = class_dir.join("BfBench.class");
    if !class.exists() {
        std::fs::write(&src, JAVA_SRC).unwrap();
        let jar = root.join("bioformats_package.jar");
        let out = Command::new("javac")
            .args([
                "-cp",
                jar.to_str().unwrap(),
                src.to_str().unwrap(),
                "-d",
                class_dir.to_str().unwrap(),
            ])
            .output()
            .expect("javac not found — install a JDK");
        if !out.status.success() {
            panic!("javac failed:\n{}", String::from_utf8_lossy(&out.stderr));
        }
    }
    class_dir
}

fn run_java_bench(root: &Path, tif: &Path) -> Duration {
    let class_dir = ensure_java_class(root);
    let jar = root.join("bioformats_package.jar");
    let cp = format!("{}:{}", jar.display(), class_dir.display());

    let warmup = 2u32;
    let measure = 5u32;

    let out = Command::new("java")
        .args([
            "-cp",
            &cp,
            "BfBench",
            tif.to_str().unwrap(),
            &warmup.to_string(),
            &measure.to_string(),
        ])
        .output()
        .expect("java not found");
    if !out.status.success() {
        panic!(
            "Java benchmark failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let ns: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("unexpected java output");
    Duration::from_nanos(ns)
}

// ── Rust benchmark ────────────────────────────────────────────────────────────

fn bench_rust(c: &mut Criterion) {
    let root = repo_root();
    let tif = root.join("test").join("tubhiswt_C0.ome.tif");
    assert!(tif.exists(), "test file not found: {}", tif.display());

    // Measure how many bytes we read per iteration (all planes)
    let bytes_per_iter: u64 = {
        let reader = bioformats::ImageReader::open(&tif).unwrap();
        let m = reader.metadata().clone();
        (m.size_x as u64)
            * (m.size_y as u64)
            * (m.image_count as u64)
            * (m.pixel_type.bytes_per_sample() as u64)
    };

    let mut group = c.benchmark_group("read_ome_tiff");
    group.throughput(Throughput::Bytes(bytes_per_iter));

    group.bench_function("rust", |b| {
        b.iter(|| {
            let mut reader = bioformats::ImageReader::open(black_box(&tif)).unwrap();
            let count = reader.metadata().image_count;
            for i in 0..count {
                let plane = reader.open_bytes(i).unwrap();
                black_box(plane);
            }
        })
    });

    // ── Java comparison (run once outside criterion, print result) ────────────
    println!("\n── Java Bio-Formats ──────────────────────────────────────");
    let java_dur = run_java_bench(&root, &tif);
    let java_throughput_mbs = (bytes_per_iter as f64 / 1_048_576.0) / java_dur.as_secs_f64();
    println!(
        "  avg time : {:.2} ms  ({:.1} MiB/s)",
        java_dur.as_secs_f64() * 1000.0,
        java_throughput_mbs,
    );
    println!("─────────────────────────────────────────────────────────\n");

    group.finish();
}

criterion_group!(benches, bench_rust);
criterion_main!(benches);
