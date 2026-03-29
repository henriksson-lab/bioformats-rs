//! bioformats-convert: convert between image formats.
//! Usage: bioformats-convert <input> <output>

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: bioformats-convert <input> <output>");
        std::process::exit(1);
    }
    let input = std::path::Path::new(&args[1]);
    let output = std::path::Path::new(&args[2]);

    // Open input
    let mut reader = bioformats::ImageReader::open(input)
        .unwrap_or_else(|e| { eprintln!("Error opening {}: {}", input.display(), e); std::process::exit(1); });

    let meta = reader.metadata().clone();
    println!("Input: {}x{}, {} planes, {:?}", meta.size_x, meta.size_y, meta.image_count, meta.pixel_type);

    // Read all planes
    let mut planes = Vec::new();
    for i in 0..meta.image_count {
        let data = reader.open_bytes(i)
            .unwrap_or_else(|e| { eprintln!("Error reading plane {}: {}", i, e); std::process::exit(1); });
        planes.push(data);
    }

    // Write output
    bioformats::ImageWriter::save(output, &meta, &planes)
        .unwrap_or_else(|e| { eprintln!("Error writing {}: {}", output.display(), e); std::process::exit(1); });

    println!("Converted {} -> {}", input.display(), output.display());
}
