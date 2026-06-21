//! Volocity (.mvd2) and Nikon NIS (.nif) format readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// --- Volocity .mvd2 -----------------------------------------------------------
//
// Volocity (PerkinElmer) stores 3D/4D microscopy data in .mvd2 files.
// Java Bio-Formats treats .mvd2 as the library root and .aisf/.aiix/.dat/.atsf
// files as companion files below the library's Data tree. The actual .mvd2
// metadata tables are Metakit-backed, so this reader translates the Java
// detection and companion routing contract but keeps pixel parsing unsupported.

const VOLOCITY_UNSUPPORTED: &str = "Volocity MVD2 native Metakit decoding is unsupported; explicit BFVOLOCITYMVD2 blind raw fixtures are supported";
const VOLOCITY_SUFFIXES: &[&str] = &["mvd2", "aisf", "aiix", "dat", "atsf"];
const VOLOCITY_BLIND_MAGIC: &[u8; 16] = b"BFVOLOCITYMVD2\0\0";
const VOLOCITY_BLIND_HEADER_LEN: usize = 48;
const VOLOCITY_METAKIT_MAX_STRUCTURE: usize = 64 * 1024;
const VOLOCITY_METAKIT_MAX_PREVIEW_BYTES: usize = 96;
const VOLOCITY_MAX_COMPANION_SCAN_ENTRIES: usize = 4096;
const VOLOCITY_MAX_COMPANION_SCAN_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy)]
struct VolocityBlindLayout {
    data_offset: usize,
    plane_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityMetakitColumn {
    name: String,
    type_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityMetakitTable {
    name: String,
    row_count: Option<usize>,
    columns: Vec<VolocityMetakitColumn>,
    scalar_values: Vec<(String, String)>,
    first_row_values: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
struct VolocityMetakitProbe {
    little_endian: bool,
    footer_offset: usize,
    toc_offset: usize,
    structure_len: usize,
    tables: Vec<VolocityMetakitTable>,
    stack_candidates: Vec<VolocityStackCandidate>,
}

/// Sibling directory next to the `.mvd2` library that holds companion pixel
/// files. Mirrors Java `VolocityReader.DATA_DIR = "Data"`.
const DATA_DIR: &str = "Data";

#[derive(Debug, Clone, PartialEq)]
struct VolocityStackCandidate {
    sample_id: i32,
    stack_name: String,
    parent_id: i32,
    name_link: Option<i32>,
    file_link: Option<i32>,
    resolved_file: Option<VolocityFileLink>,
    pixels_dat: Option<String>,
    channel_child_sample_id: Option<i32>,
    channel_count: Option<usize>,
    channel_links: Vec<VolocityChannelLink>,
    inline_data_len: usize,
    /// The stack sample's own inline data (Java `sampleTable[row][13]`). Used as
    /// the EMBEDDED_STREAM fallback when a non-channel stack's `.dat` file is
    /// missing (Java initFile lines 360-379).
    inline_data: Option<Vec<u8>>,
    native_stream_clue: Option<VolocityNativeStreamClue>,
    external_data: Option<i32>,
    metadata: VolocityStackMetadata,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct VolocityStackMetadata {
    timestamp_atsf_id: Option<i32>,
    physical_x: Option<f64>,
    physical_y: Option<f64>,
    physical_z: Option<f64>,
    magnification: Option<f64>,
    detector: Option<String>,
    description: Option<String>,
    x_location: Option<f64>,
    y_location: Option<f64>,
    z_location: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityChannelLink {
    sample_id: i32,
    name: String,
    aisf_id: Option<i32>,
    pixels_dat: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityFileLink {
    file_id: i32,
    name: Option<String>,
    spec_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityNativeStreamClue {
    little_endian: bool,
    size_x: i32,
    size_y: i32,
    size_z: i32,
    stream_len: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum VolocityMetakitValue {
    String(String),
    Integer(i32),
    Float(f32),
    Double(f64),
    Bytes(Vec<u8>),
    Long(i64),
}

impl From<crate::metakit::Value> for VolocityMetakitValue {
    fn from(value: crate::metakit::Value) -> Self {
        match value {
            crate::metakit::Value::String(value) => Self::String(value),
            crate::metakit::Value::Integer(value) => Self::Integer(value),
            crate::metakit::Value::Float(value) => Self::Float(value),
            crate::metakit::Value::Double(value) => Self::Double(value),
            crate::metakit::Value::Bytes(value) => Self::Bytes(value),
            crate::metakit::Value::Long(value) => Self::Long(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocitySampleRow {
    id: i32,
    parent: i32,
    child_type: i32,
    file_link: Option<i32>,
    name_link: Option<i32>,
    inline_data_len: usize,
    inline_data: Option<Vec<u8>>,
    external_data: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityStringRow {
    id: i32,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VolocityFileRow {
    id: i32,
    name: Option<String>,
    spec: Option<Vec<u8>>,
}

fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn is_volocity_companion_suffix(ext: Option<&str>) -> bool {
    matches!(ext, Some(suffix) if VOLOCITY_SUFFIXES[1..].contains(&suffix))
}

fn volocity_library_from_companion(path: &Path) -> Option<PathBuf> {
    // Java VolocityReader walks three parents from a companion file and then
    // expects "<library>/<library>.mvd2".
    let library_dir = path.parent()?.parent()?.parent()?;
    let library_name = library_dir.file_name()?;
    let candidate = library_dir.join(format!("{}.mvd2", library_name.to_string_lossy()));
    candidate.exists().then_some(candidate)
}

fn volocity_library_from_companion_for_init(path: &Path) -> Option<PathBuf> {
    // Java initFile(String) uses a different, looser companion route than
    // isThisType: climb two parents, recursively list that directory, and use
    // the first .mvd2 found there.
    let search_root = path.parent()?.parent()?;
    let mut budget = VOLOCITY_MAX_COMPANION_SCAN_ENTRIES;
    volocity_find_mvd2(search_root, VOLOCITY_MAX_COMPANION_SCAN_DEPTH, &mut budget)
}

fn volocity_find_mvd2(dir: &Path, depth: usize, budget: &mut usize) -> Option<PathBuf> {
    if depth == 0 || *budget == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    paths.sort();

    for path in &paths {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        if ext_lower(path).as_deref() == Some("mvd2") {
            return Some(path.clone());
        }
    }
    for path in paths {
        if path.is_dir() {
            if let Some(found) = volocity_find_mvd2(&path, depth - 1, budget) {
                return Some(found);
            }
        }
    }
    None
}

fn volocity_error(path: Option<&Path>) -> BioFormatsError {
    let detail = match path {
        Some(path) => format!("{VOLOCITY_UNSUPPORTED}: {}", path.display()),
        None => VOLOCITY_UNSUPPORTED.to_string(),
    };
    BioFormatsError::UnsupportedFormat(detail)
}

fn volocity_native_error(path: &Path, probe: &VolocityMetakitProbe) -> BioFormatsError {
    let endian = if probe.little_endian {
        "little-endian"
    } else {
        "big-endian"
    };
    let tables = if probe.tables.is_empty() {
        "no tables reported".to_string()
    } else {
        probe
            .tables
            .iter()
            .map(|table| {
                let columns = if table.columns.is_empty() {
                    "no columns".to_string()
                } else {
                    table
                        .columns
                        .iter()
                        .map(|column| format!("{}:{}", column.name, column.type_string))
                        .collect::<Vec<_>>()
                        .join("|")
                };
                match table.row_count {
                    Some(rows) => format!("{}({rows})[{columns}]", table.name),
                    None => format!("{}(?)[{columns}]", table.name),
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let scalars = probe
        .tables
        .iter()
        .flat_map(|table| {
            table
                .scalar_values
                .iter()
                .map(move |(column, value)| format!("{}.{}={value}", table.name, column))
        })
        .collect::<Vec<_>>();
    let scalar_summary = if scalars.is_empty() {
        " no single-row scalar values decoded".to_string()
    } else {
        format!(" single-row scalars: {}", scalars.join(", "))
    };
    let semantic_summary = volocity_native_semantic_summary(probe)
        .map(|summary| format!(" Java metadata roles: {summary}"))
        .unwrap_or_default();
    let companion_summary = volocity_companion_provenance_summary(path, probe)
        .map(|summary| format!(" companion provenance: {summary};"))
        .unwrap_or_default();
    BioFormatsError::UnsupportedFormat(format!(
        "{VOLOCITY_UNSUPPORTED}; detected native Metakit {endian} footer={} toc={} structure={}B table_count={} tables: {tables};{scalar_summary};{semantic_summary};{companion_summary}: {}",
        probe.footer_offset,
        probe.toc_offset,
        probe.structure_len,
        probe.tables.len(),
        path.display()
    ))
}

fn volocity_native_semantic_summary(probe: &VolocityMetakitProbe) -> Option<String> {
    const ROLE_TABLES: &[(&str, &str)] = &[
        ("variables", "variablesView"),
        ("samples", "samplesViewR"),
        ("strings", "stringsViewR"),
        ("files", "filesViewR"),
    ];
    const VARIABLE_ALIASES: &[(&str, &str)] = &[
        ("varVersion", "version"),
        ("varNextSampleID", "next_sample_id"),
        ("varNextStringID", "next_string_id"),
        ("varNextFileID", "next_file_id"),
        ("varDemoKey", "demo_key"),
    ];

    let mut parts = Vec::new();
    let mut roles = Vec::new();
    for (role, table_name) in ROLE_TABLES {
        if let Some(table) = probe.tables.iter().find(|table| table.name == *table_name) {
            let rows = table
                .row_count
                .map(|rows| rows.to_string())
                .unwrap_or_else(|| "?".to_string());
            roles.push(format!("{role}={table_name}({rows})"));
        }
    }
    if !roles.is_empty() {
        parts.push(roles.join(", "));
    }

    if let Some(variables) = probe
        .tables
        .iter()
        .find(|table| table.name == "variablesView")
    {
        let mut aliases = Vec::new();
        for (source, alias) in VARIABLE_ALIASES {
            if let Some((_, value)) = variables
                .scalar_values
                .iter()
                .find(|(column, _)| column == source)
            {
                aliases.push(format!("{alias}={value}"));
            }
        }
        if !aliases.is_empty() {
            parts.push(format!("variables {}", aliases.join(", ")));
        }
    }

    if let Some(samples) = probe
        .tables
        .iter()
        .find(|table| table.name == "samplesViewR")
    {
        let hierarchy_columns = [
            ("sampleID", "id"),
            ("sampleParent", "parent"),
            ("sampleChildType", "child_type"),
            ("sampleChildPos", "child_pos"),
            ("sampleOrigChildPos", "original_child_pos"),
            ("sampleFileLink", "file_link"),
            ("sampleNameLink", "name_link"),
            ("sampleData", "inline_data"),
            ("sampleExternalData", "external_data"),
        ];
        let roles = hierarchy_columns
            .iter()
            .filter_map(|(column_name, role)| {
                samples
                    .columns
                    .iter()
                    .any(|column| column.name == *column_name)
                    .then(|| format!("{role}={column_name}"))
            })
            .collect::<Vec<_>>();
        if !roles.is_empty() {
            parts.push(format!("sample hierarchy {}", roles.join(", ")));
        }
        if !samples.first_row_values.is_empty() {
            parts.push(format!(
                "first sample row {}",
                volocity_sample_row_summary(&samples.first_row_values)
            ));
        }
    }

    for (table_name, label, expected_columns) in [
        (
            "stringsViewR",
            "string links",
            &["stringID", "stringString", "stringRefCount"][..],
        ),
        (
            "filesViewR",
            "file links",
            &["fileID", "fileName", "fileSpec", "fileRefCount"][..],
        ),
    ] {
        if let Some(table) = probe.tables.iter().find(|table| table.name == table_name) {
            let columns = expected_columns
                .iter()
                .filter(|column_name| {
                    table
                        .columns
                        .iter()
                        .any(|column| column.name == **column_name)
                })
                .copied()
                .collect::<Vec<_>>();
            if !columns.is_empty() {
                parts.push(format!("{label} {table_name}[{}]", columns.join(", ")));
            }
            if !table.first_row_values.is_empty() {
                parts.push(format!(
                    "first {label} row {}",
                    volocity_metakit_row_summary(&table.first_row_values)
                ));
            }
        }
    }

    if !probe.stack_candidates.is_empty() {
        let candidates = probe
            .stack_candidates
            .iter()
            .take(8)
            .map(|candidate| {
                let mut details = vec![
                    format!("sampleID={}", candidate.sample_id),
                    format!("name=\"{}\"", candidate.stack_name.escape_debug()),
                    format!("parent={}", candidate.parent_id),
                    format!("inline_data={}B", candidate.inline_data_len),
                ];
                if let Some(name_link) = candidate.name_link {
                    details.push(format!("name_link={name_link}"));
                }
                if let Some(file_link) = candidate.file_link {
                    details.push(format!("file_link={file_link}"));
                }
                if let Some(file) = &candidate.resolved_file {
                    let mut file_details = vec![format!("fileID={}", file.file_id)];
                    if let Some(name) = &file.name {
                        file_details.push(format!("name=\"{}\"", name.escape_debug()));
                    }
                    if let Some(spec_preview) = &file.spec_preview {
                        file_details.push(format!("spec={spec_preview}"));
                    }
                    details.push(format!("file=[{}]", file_details.join(" ")));
                }
                if let Some(pixels_dat) = &candidate.pixels_dat {
                    details.push(format!("pixels_dat={pixels_dat}"));
                }
                if let Some(channel_child) = candidate.channel_child_sample_id {
                    details.push(format!("channels_child={channel_child}"));
                }
                if let Some(channel_count) = candidate.channel_count {
                    details.push(format!("channels={channel_count}"));
                }
                if !candidate.channel_links.is_empty() {
                    let channel_links = candidate
                        .channel_links
                        .iter()
                        .take(8)
                        .map(|channel| {
                            let mut channel_details = vec![
                                format!("sampleID={}", channel.sample_id),
                                format!("name=\"{}\"", channel.name.escape_debug()),
                            ];
                            if let Some(aisf_id) = channel.aisf_id {
                                channel_details.push(format!("aisf_id={aisf_id}"));
                            }
                            if let Some(pixels_dat) = &channel.pixels_dat {
                                channel_details.push(format!("pixels_dat={pixels_dat}"));
                            }
                            channel_details.join(" ")
                        })
                        .collect::<Vec<_>>();
                    let suffix = if candidate.channel_links.len() > channel_links.len() {
                        format!(
                            ", ... {} more",
                            candidate.channel_links.len() - channel_links.len()
                        )
                    } else {
                        String::new()
                    };
                    details.push(format!(
                        "channel_links=[{}{}]",
                        channel_links.join(", "),
                        suffix
                    ));
                }
                if let Some(clue) = &candidate.native_stream_clue {
                    let endian = if clue.little_endian { "LE" } else { "BE" };
                    details.push(format!(
                        "native_stream={}x{}x{} {endian} len={}B",
                        clue.size_x, clue.size_y, clue.size_z, clue.stream_len
                    ));
                }
                if let Some(external_data) = candidate.external_data {
                    details.push(format!("external_data={external_data}"));
                }
                let metadata = &candidate.metadata;
                if let Some(timestamp_atsf_id) = metadata.timestamp_atsf_id {
                    details.push(format!("timestamp_atsf={timestamp_atsf_id}.atsf"));
                }
                if let Some(physical_x) = metadata.physical_x {
                    details.push(format!("physicalX={physical_x}"));
                }
                if let Some(physical_y) = metadata.physical_y {
                    details.push(format!("physicalY={physical_y}"));
                }
                if let Some(physical_z) = metadata.physical_z {
                    details.push(format!("physicalZ={physical_z}"));
                }
                if let Some(magnification) = metadata.magnification {
                    details.push(format!("magnification={magnification}"));
                }
                if let Some(detector) = &metadata.detector {
                    details.push(format!("detector=\"{}\"", detector.escape_debug()));
                }
                if let Some(description) = &metadata.description {
                    details.push(format!("description=\"{}\"", description.escape_debug()));
                }
                if let Some(x_location) = metadata.x_location {
                    details.push(format!("xLocation={x_location}"));
                }
                if let Some(y_location) = metadata.y_location {
                    details.push(format!("yLocation={y_location}"));
                }
                if let Some(z_location) = metadata.z_location {
                    details.push(format!("zLocation={z_location}"));
                }
                details.join(", ")
            })
            .collect::<Vec<_>>();
        let suffix = if probe.stack_candidates.len() > candidates.len() {
            format!(
                ", ... {} more",
                probe.stack_candidates.len() - candidates.len()
            )
        } else {
            String::new()
        };
        parts.push(format!(
            "Java stack candidates {}: {}{}",
            probe.stack_candidates.len(),
            candidates.join("; "),
            suffix
        ));
    }

    (!parts.is_empty()).then(|| parts.join("; "))
}

fn volocity_companion_provenance_summary(
    library_root: &Path,
    probe: &VolocityMetakitProbe,
) -> Option<String> {
    let data_dir = library_root.parent()?.join("Data");
    let mut parts = Vec::new();
    parts.push(format!(
        "Data directory {}",
        if data_dir.is_dir() {
            "present"
        } else {
            "missing"
        }
    ));

    let mut requests = Vec::new();
    for candidate in &probe.stack_candidates {
        if let Some(external_data) = candidate.external_data {
            requests.push((
                format!(
                    "stack sampleID={} external_data={external_data}",
                    candidate.sample_id
                ),
                format!("{external_data}.aisf"),
            ));
            requests.push((
                format!(
                    "stack sampleID={} external_data={external_data}",
                    candidate.sample_id
                ),
                format!("{external_data}.aiix"),
            ));
            requests.push((
                format!(
                    "stack sampleID={} external_data={external_data}",
                    candidate.sample_id
                ),
                format!("{external_data}.dat"),
            ));
            requests.push((
                format!(
                    "stack sampleID={} external_data={external_data}",
                    candidate.sample_id
                ),
                format!("{external_data}.atsf"),
            ));
        }
        if let Some(timestamp_atsf_id) = candidate.metadata.timestamp_atsf_id {
            requests.push((
                format!(
                    "stack sampleID={} timestamp_atsf={timestamp_atsf_id}",
                    candidate.sample_id
                ),
                format!("{timestamp_atsf_id}.atsf"),
            ));
        }
        if let Some(pixels_dat) = &candidate.pixels_dat {
            requests.push((
                format!("stack sampleID={} pixels_dat", candidate.sample_id),
                pixels_dat.clone(),
            ));
        }
        for channel in &candidate.channel_links {
            if let Some(aisf_id) = channel.aisf_id {
                requests.push((
                    format!("channel sampleID={} aisf_id={aisf_id}", channel.sample_id),
                    format!("{aisf_id}.aisf"),
                ));
            }
            if let Some(pixels_dat) = &channel.pixels_dat {
                requests.push((
                    format!("channel sampleID={} pixels_dat", channel.sample_id),
                    pixels_dat.clone(),
                ));
            }
        }
    }

    if requests.is_empty() {
        return None;
    }

    let mut scan_budget = VOLOCITY_MAX_COMPANION_SCAN_ENTRIES;
    for (label, filename) in requests.into_iter().take(16) {
        let found = if data_dir.is_dir() {
            volocity_find_companion_file(
                &data_dir,
                &filename,
                VOLOCITY_MAX_COMPANION_SCAN_DEPTH,
                &mut scan_budget,
            )
        } else {
            None
        };
        let status = found
            .as_ref()
            .map(|path| volocity_relative_display(library_root, path))
            .unwrap_or_else(|| "missing".to_string());
        parts.push(format!("{label} {filename}={status}"));
    }

    Some(parts.join(", "))
}

fn volocity_relative_display(library_root: &Path, path: &Path) -> String {
    let base = library_root.parent().unwrap_or_else(|| Path::new(""));
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn volocity_find_companion_file(
    dir: &Path,
    filename: &str,
    depth: usize,
    budget: &mut usize,
) -> Option<PathBuf> {
    if depth == 0 || *budget == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    paths.sort();

    for path in &paths {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        if path.file_name().and_then(|name| name.to_str()) == Some(filename) {
            return Some(path.clone());
        }
    }
    for path in paths {
        if path.is_dir() {
            if let Some(found) = volocity_find_companion_file(&path, filename, depth - 1, budget) {
                return Some(found);
            }
        }
    }
    None
}

fn volocity_sample_row_summary(values: &[(String, String)]) -> String {
    volocity_metakit_row_summary_with_child_type(values, true)
}

fn volocity_metakit_row_summary(values: &[(String, String)]) -> String {
    volocity_metakit_row_summary_with_child_type(values, false)
}

fn volocity_metakit_row_summary_with_child_type(
    values: &[(String, String)],
    annotate_child_type: bool,
) -> String {
    values
        .iter()
        .map(|(column, value)| {
            if annotate_child_type && column == "sampleChildType" {
                format!("{column}={value} ({})", volocity_sample_child_type(value))
            } else {
                format!("{column}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn volocity_sample_child_type(value: &str) -> &'static str {
    match value.parse::<i32>().ok() {
        Some(1) => "Java stack-candidate branch",
        _ => "unknown Java sample child type",
    }
}

fn volocity_metakit_probe_error(path: &Path, reason: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{VOLOCITY_UNSUPPORTED}; Metakit stream signature was present but metadata probe failed: {reason}: {}",
        path.display()
    ))
}

fn volocity_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn volocity_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn metakit_i32_be_at(bytes: &[u8], offset: usize) -> std::result::Result<i32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "integer offset overflows".to_string())?;
    let data = bytes
        .get(offset..end)
        .ok_or_else(|| format!("truncated i32 at offset {offset}"))?;
    Ok(i32::from_be_bytes([data[0], data[1], data[2], data[3]]))
}

fn metakit_read_byte(bytes: &[u8], offset: &mut usize) -> std::result::Result<u8, String> {
    let byte = *bytes
        .get(*offset)
        .ok_or_else(|| format!("unexpected EOF at offset {}", *offset))?;
    *offset += 1;
    Ok(byte)
}

fn metakit_read_bp_int(bytes: &[u8], offset: &mut usize) -> std::result::Result<i32, String> {
    let sign_byte = metakit_read_byte(bytes, offset)?;
    let negative = sign_byte == 0;
    let data_byte = if negative {
        metakit_read_byte(bytes, offset)?
    } else {
        sign_byte
    };
    let mut stop_byte = data_byte;
    let mut data_bytes = Vec::new();

    while (stop_byte & 0x80) == 0 {
        if data_bytes.len() >= 4 {
            return Err("overlong byte-packed integer".to_string());
        }
        data_bytes.push(stop_byte);
        stop_byte = metakit_read_byte(bytes, offset)?;
    }

    let mut value = 0i32;
    for (index, byte) in data_bytes.iter().enumerate() {
        let shift = (data_bytes.len() - index) * 7;
        value |= i32::from(*byte) << shift;
    }
    value |= i32::from(stop_byte & 0x7f);

    if negative {
        value = !value;
    }
    Ok(value)
}

fn metakit_read_p_string(bytes: &[u8], offset: &mut usize) -> std::result::Result<String, String> {
    let len = metakit_read_bp_int(bytes, offset)?;
    if len < 0 {
        return Err(format!("negative structure string length: {len}"));
    }
    let len = len as usize;
    if len > VOLOCITY_METAKIT_MAX_STRUCTURE {
        return Err(format!(
            "structure string length {len} exceeds safety limit"
        ));
    }
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "structure string end overflows".to_string())?;
    let data = bytes
        .get(*offset..end)
        .ok_or_else(|| "truncated structure string".to_string())?;
    *offset = end;
    std::str::from_utf8(data)
        .map(str::to_owned)
        .map_err(|err| format!("structure string is not UTF-8: {err}"))
}

fn metakit_format_bytes_preview(bytes: &[u8], total_len: usize) -> String {
    let preview = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("");
    if total_len > bytes.len() {
        format!("{total_len} bytes hex={preview}...")
    } else {
        format!("{total_len} bytes hex={preview}")
    }
}

fn volocity_value_i32(value: &Option<VolocityMetakitValue>) -> Option<i32> {
    match value {
        Some(VolocityMetakitValue::Integer(value)) => Some(*value),
        _ => None,
    }
}

fn volocity_metakit_value_summary(value: &VolocityMetakitValue) -> String {
    match value {
        VolocityMetakitValue::String(value) => {
            let preview = value.replace('\0', "\\0");
            format!("\"{}\"", preview.chars().take(48).collect::<String>())
        }
        VolocityMetakitValue::Integer(value) => value.to_string(),
        VolocityMetakitValue::Float(value) => value.to_string(),
        VolocityMetakitValue::Double(value) => value.to_string(),
        VolocityMetakitValue::Bytes(bytes) => {
            let preview_len = bytes.len().min(VOLOCITY_METAKIT_MAX_PREVIEW_BYTES);
            metakit_format_bytes_preview(&bytes[..preview_len], bytes.len())
        }
        VolocityMetakitValue::Long(value) => value.to_string(),
    }
}

fn volocity_metakit_row_scalar_summary(
    row: &[Option<VolocityMetakitValue>],
    columns: &[VolocityMetakitColumn],
) -> Vec<(String, String)> {
    columns
        .iter()
        .zip(row.iter())
        .filter_map(|(column, value)| {
            value
                .as_ref()
                .map(|value| (column.name.clone(), volocity_metakit_value_summary(value)))
        })
        .collect()
}

fn volocity_column_index(columns: &[VolocityMetakitColumn], name: &str) -> Option<usize> {
    columns.iter().position(|column| column.name == name)
}

fn volocity_i32_column(
    row: &[Option<VolocityMetakitValue>],
    columns: &[VolocityMetakitColumn],
    name: &str,
) -> Option<i32> {
    volocity_column_index(columns, name)
        .and_then(|index| row.get(index).and_then(volocity_value_i32))
}

fn volocity_bytes_column(
    row: &[Option<VolocityMetakitValue>],
    columns: &[VolocityMetakitColumn],
    name: &str,
) -> Option<Vec<u8>> {
    volocity_column_index(columns, name).and_then(|index| match row.get(index) {
        Some(Some(VolocityMetakitValue::Bytes(bytes))) => Some(bytes.clone()),
        _ => None,
    })
}

fn volocity_string_column(
    row: &[Option<VolocityMetakitValue>],
    columns: &[VolocityMetakitColumn],
    name: &str,
) -> Option<String> {
    volocity_column_index(columns, name).and_then(|index| match row.get(index) {
        Some(Some(VolocityMetakitValue::String(value))) => Some(value.clone()),
        _ => None,
    })
}

fn volocity_sample_rows_from_metakit(
    rows: &[Vec<Option<VolocityMetakitValue>>],
    columns: &[VolocityMetakitColumn],
) -> Vec<VolocitySampleRow> {
    rows.iter()
        .filter_map(|row| {
            let inline_data = volocity_bytes_column(row, columns, "sampleData");
            Some(VolocitySampleRow {
                id: volocity_i32_column(row, columns, "sampleID")?,
                parent: volocity_i32_column(row, columns, "sampleParent")?,
                child_type: volocity_i32_column(row, columns, "sampleChildType")?,
                file_link: volocity_i32_column(row, columns, "sampleFileLink"),
                name_link: volocity_i32_column(row, columns, "sampleNameLink"),
                inline_data_len: inline_data.as_ref().map_or(0, Vec::len),
                inline_data,
                external_data: volocity_i32_column(row, columns, "sampleExternalData"),
            })
        })
        .collect()
}

fn volocity_file_rows_from_metakit(
    rows: &[Vec<Option<VolocityMetakitValue>>],
    columns: &[VolocityMetakitColumn],
) -> Vec<VolocityFileRow> {
    rows.iter()
        .filter_map(|row| {
            Some(VolocityFileRow {
                id: volocity_i32_column(row, columns, "fileID")?,
                name: volocity_string_column(row, columns, "fileName"),
                spec: volocity_bytes_column(row, columns, "fileSpec"),
            })
        })
        .collect()
}

fn volocity_string_rows_from_metakit(
    rows: &[Vec<Option<VolocityMetakitValue>>],
    columns: &[VolocityMetakitColumn],
) -> Vec<VolocityStringRow> {
    rows.iter()
        .filter_map(|row| {
            Some(VolocityStringRow {
                id: volocity_i32_column(row, columns, "stringID")?,
                value: volocity_string_column(row, columns, "stringString")?,
            })
        })
        .collect()
}

fn volocity_java_trim(value: &str) -> String {
    value.trim_matches(|c: char| c <= ' ').to_string()
}

fn volocity_lookup_string(strings: &[VolocityStringRow], string_id: Option<i32>) -> Option<String> {
    let string_id = string_id?;
    strings
        .iter()
        .find(|row| row.id == string_id)
        .map(|row| volocity_java_trim(&row.value))
}

fn volocity_resolve_file_link(
    files: &[VolocityFileRow],
    file_id: Option<i32>,
) -> Option<VolocityFileLink> {
    let file_id = file_id?;
    let row = files.iter().find(|row| row.id == file_id)?;
    let spec_preview = row.spec.as_deref().map(|bytes| {
        let preview_len = bytes.len().min(VOLOCITY_METAKIT_MAX_PREVIEW_BYTES);
        metakit_format_bytes_preview(&bytes[..preview_len], bytes.len())
    });
    Some(VolocityFileLink {
        file_id: row.id,
        name: row.name.as_deref().map(volocity_java_trim),
        spec_preview,
    })
}

fn volocity_get_file(samples: &[VolocitySampleRow], parent: i32) -> Option<String> {
    // Java getFile(parent, dir): the sample whose ID matches `parent` has its
    // sampleTable[row][14] (== sampleExternalData) resolved as "<value>.dat"
    // beneath the Data directory. Java only emits a path when [14] is non-null
    // (any value, including 0). We surface the bare "<value>.dat" leaf name; the
    // caller joins it with the Data directory.
    for row in samples {
        if row.id == parent {
            if let Some(external) = row.external_data {
                return Some(format!("{external}.dat"));
            }
        }
    }
    None
}

fn volocity_get_child<'a>(
    samples: &'a [VolocitySampleRow],
    strings: &[VolocityStringRow],
    parent_id: i32,
    child_name: &str,
) -> Option<&'a VolocitySampleRow> {
    samples.iter().find(|row| {
        row.parent == parent_id
            && volocity_lookup_string(strings, row.name_link).as_deref() == Some(child_name)
    })
}

fn volocity_child_count(samples: &[VolocitySampleRow], parent_id: i32) -> usize {
    samples.iter().filter(|row| row.parent == parent_id).count()
}

fn volocity_children<'a>(
    samples: &'a [VolocitySampleRow],
    parent_id: i32,
) -> impl Iterator<Item = &'a VolocitySampleRow> {
    samples.iter().filter(move |row| row.parent == parent_id)
}

fn volocity_read_stream_i32(bytes: &[u8], offset: usize, little_endian: bool) -> Option<i32> {
    let data = bytes.get(offset..offset.checked_add(4)?)?;
    Some(if little_endian {
        i32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        i32::from_be_bytes([data[0], data[1], data[2], data[3]])
    })
}

fn volocity_read_stream_f64(bytes: &[u8], offset: usize, little_endian: bool) -> Option<f64> {
    let data = bytes.get(offset..offset.checked_add(8)?)?;
    let array = [
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ];
    Some(if little_endian {
        f64::from_le_bytes(array)
    } else {
        f64::from_be_bytes(array)
    })
}

fn volocity_read_stream_string(bytes: &[u8], offset: usize, little_endian: bool) -> Option<String> {
    let len = volocity_read_stream_i32(bytes, offset, little_endian)?;
    if len < 0 {
        return None;
    }
    let len = usize::try_from(len).ok()?;
    let start = offset.checked_add(4)?;
    let end = start.checked_add(len)?;
    let data = bytes.get(start..end)?;
    Some(volocity_java_trim(&String::from_utf8_lossy(data)))
}

fn volocity_native_stream_clue(bytes: &[u8]) -> Option<VolocityNativeStreamClue> {
    let little_endian = bytes.first().copied() == Some(b'I');
    let size_x = volocity_read_stream_i32(bytes, 22, little_endian)?;
    let size_y = volocity_read_stream_i32(bytes, 26, little_endian)?;
    let size_z = volocity_read_stream_i32(bytes, 30, little_endian)?;
    let pixels = i64::from(size_x)
        .checked_mul(i64::from(size_y))?
        .checked_mul(i64::from(size_z))?;
    if pixels <= 0 || pixels >= (bytes.len() as i64).checked_mul(3)? {
        return None;
    }
    Some(VolocityNativeStreamClue {
        little_endian,
        size_x,
        size_y,
        size_z,
        stream_len: bytes.len(),
    })
}

/// Java `getStream(int row)` — returns the bytes of the sample row's data
/// stream. When `sampleExternalData` (col 14) is null or "0", the inline
/// `sampleData` (col 13) bytes are used; otherwise the sibling
/// `<sampleExternalData>.dat` file in the Data directory is opened.
///
/// `data_dir` is `None` during the lightweight `isThisType` probe (no library
/// directory is known yet); in that case external rows fall back to their
/// inline data, matching the pre-existing inline-only behavior.
fn volocity_get_stream(row: &VolocitySampleRow, data_dir: Option<&Path>) -> Option<Vec<u8>> {
    // Java: fileLink = o == null ? "0" : o.toString().trim(); the integer 0
    // stringifies to "0", so external_data == Some(0) also reads inline.
    let external = row.external_data.filter(|value| *value != 0);
    match (external, data_dir) {
        (Some(file_link), Some(dir)) => {
            let path = dir.join(format!("{file_link}.dat"));
            std::fs::read(&path).ok()
        }
        // No external link (or no known data dir) → inline sampleData.
        _ => row.inline_data.clone(),
    }
}

fn volocity_channel_aisf_id(bytes: Option<&[u8]>) -> Option<i32> {
    // Java getStream(channel).seek(22).readInt() maps this ID to <id>.aisf.
    volocity_read_stream_i32(bytes?, 22, true)
}

fn volocity_parent_name(
    samples: &[VolocitySampleRow],
    strings: &[VolocityStringRow],
    mut parent_id: i32,
) -> String {
    let mut parent_name = String::new();
    let mut guard = 0usize;
    while parent_id != 1 && guard < samples.len() {
        guard += 1;
        let original_id = parent_id;
        if let Some(row) = samples.iter().find(|row| row.id == parent_id) {
            if let Some(name) = volocity_lookup_string(strings, row.name_link) {
                parent_name = format!("{name}/{parent_name}");
            }
            parent_id = row.parent;
        }
        if parent_id == original_id {
            break;
        }
    }
    parent_name
}

fn volocity_stack_candidates(
    samples: &[VolocitySampleRow],
    strings: &[VolocityStringRow],
    files: &[VolocityFileRow],
    data_dir: Option<&Path>,
) -> Vec<VolocityStackCandidate> {
    samples
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            if index == 0 || row.child_type != 1 {
                return None;
            }

            let channel_child = volocity_get_child(samples, strings, row.id, "Channels");
            let has_external_data = row.external_data.is_some_and(|value| value != 0);
            // Java qualification (line 295-297): channelIndex>=0
            //   OR sampleExternalData (col 14) != 0
            //   OR sampleData (col 13) length > 21.
            let inline_len = row.inline_data.as_ref().map_or(0, Vec::len);
            let qualifies = channel_child.is_some() || has_external_data || inline_len > 21;
            if !qualifies {
                return None;
            }

            // Java line 301-316: when there is no "Channels" child, the stack is
            // only kept if the data stream's x*y*z is in a sane range. Java reads
            // this from getStream(i) which uses the external `.dat` when
            // sampleExternalData != 0, otherwise the inline sampleData. We thread
            // data_dir through so external-stream stacks are validated against the
            // real `.dat` like Java; when no data_dir is known (the isThisType
            // probe) external rows fall back to inline bytes.
            let row_stream = volocity_get_stream(row, data_dir);
            let native_stream_clue = row_stream.as_deref().and_then(volocity_native_stream_clue);
            // Java line 301-316: the x*y*z sanity check is a REJECTION filter that
            // only fires when getStream(i) actually yields readable bytes. If a
            // stream is readable but its x*y*z is out of the sane range, the stack
            // is dropped (native_stream_clue is None because the clue helper already
            // encodes the `0 < x*y*z < len*3` predicate). But when no stream is
            // available at all (external `.dat` absent / bounded byte-only probe,
            // or only an empty inline fallback with no dimensions to read), the
            // stack has still qualified via the predicate above and must be KEPT —
            // there is nothing to validate, so Java would not reach the rejection
            // branch with usable dimensions.
            let stream_readable = row_stream.as_ref().is_some_and(|bytes| !bytes.is_empty());
            if channel_child.is_none() && stream_readable && native_stream_clue.is_none() {
                return None;
            }

            let parent_name = volocity_parent_name(samples, strings, row.parent);
            let name = volocity_lookup_string(strings, row.name_link).unwrap_or_default();
            // Java initFile per-channel pixels-file resolution: when the channel
            // stream is longer than 22 bytes the .aisf id is read at offset 22,
            // otherwise the pixels file falls back to getFile(firstChild, dir).
            let channel_links = channel_child
                .into_iter()
                .flat_map(|child| volocity_children(samples, child.id))
                .map(|child| {
                    // Java getStream(channels[c]): external `.dat` when linked.
                    let child_stream = volocity_get_stream(child, data_dir);
                    let stream_len = child_stream.as_ref().map_or(0, Vec::len);
                    let (aisf_id, pixels_dat) = if stream_len > 22 {
                        (volocity_channel_aisf_id(child_stream.as_deref()), None)
                    } else {
                        let first_child = volocity_children(samples, child.id).next().map(|c| c.id);
                        (
                            None,
                            first_child.and_then(|id| volocity_get_file(samples, id)),
                        )
                    };
                    VolocityChannelLink {
                        sample_id: child.id,
                        name: volocity_lookup_string(strings, child.name_link).unwrap_or_default(),
                        aisf_id,
                        pixels_dat,
                    }
                })
                .collect::<Vec<_>>();

            // Java initFile non-channel pixels file: getFile(parent, dir) → <link>.dat.
            let pixels_dat = if channel_child.is_none() {
                volocity_get_file(samples, row.id)
            } else {
                None
            };

            // Java initFile named-child metadata streams (seek(SIGNATURE_SIZE)).
            const SIGNATURE_SIZE: usize = 13;
            let mut metadata = VolocityStackMetadata::default();
            if let Some(child) =
                volocity_get_child(samples, strings, row.id, "Timepoint times stream")
            {
                metadata.timestamp_atsf_id = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_i32(data, 22, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "um/pixel (X)") {
                metadata.physical_x = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "um/pixel (Y)") {
                metadata.physical_y = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "um/pixel (Z)") {
                metadata.physical_z = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) =
                volocity_get_child(samples, strings, row.id, "Microscope Objective")
            {
                metadata.magnification = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "Camera/Detector") {
                metadata.detector = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_string(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) =
                volocity_get_child(samples, strings, row.id, "Experiment Description")
            {
                metadata.description = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_string(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "X Location") {
                metadata.x_location = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "Y Location") {
                metadata.y_location = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }
            if let Some(child) = volocity_get_child(samples, strings, row.id, "Z Location") {
                metadata.z_location = volocity_get_stream(child, data_dir)
                    .as_deref()
                    .and_then(|data| volocity_read_stream_f64(data, SIGNATURE_SIZE, true));
            }

            Some(VolocityStackCandidate {
                sample_id: row.id,
                stack_name: format!("{parent_name}{name}"),
                parent_id: row.parent,
                name_link: row.name_link,
                file_link: row.file_link,
                resolved_file: volocity_resolve_file_link(files, row.file_link),
                pixels_dat,
                channel_child_sample_id: channel_child.map(|child| child.id),
                channel_count: channel_child.map(|child| volocity_child_count(samples, child.id)),
                channel_links,
                inline_data_len: row.inline_data_len,
                inline_data: row.inline_data.clone(),
                native_stream_clue,
                external_data: row.external_data.filter(|value| *value != 0),
                metadata,
            })
        })
        .collect()
}

fn probe_volocity_metakit(
    bytes: &[u8],
    data_dir: Option<&Path>,
) -> std::result::Result<Option<VolocityMetakitProbe>, String> {
    let little_endian = match bytes.get(0..2) {
        Some(b"JL") => true,
        Some(b"LJ") => false,
        _ => return Ok(None),
    };
    if bytes.len() < 20 {
        return Err("Metakit header is truncated".to_string());
    }
    if bytes[2] != 26 {
        return Err(format!("Metakit valid flag was {}, expected 26", bytes[2]));
    }
    if bytes[3] != 0 {
        return Err(format!("Metakit header type was {}, expected 0", bytes[3]));
    }

    let footer_pointer = metakit_i32_be_at(bytes, 4)? as i64 - 16;
    if footer_pointer < 0 {
        return Err(format!("negative footer pointer: {footer_pointer}"));
    }
    let footer_pointer =
        usize::try_from(footer_pointer).map_err(|_| "footer pointer overflows".to_string())?;
    let footer_end = footer_pointer
        .checked_add(16)
        .ok_or_else(|| "footer end overflows".to_string())?;
    if footer_end > bytes.len() {
        return Err(format!("footer at offset {footer_pointer} is outside file"));
    }

    let toc_location = metakit_i32_be_at(bytes, footer_pointer + 12)?;
    if toc_location < 0 {
        return Err(format!("negative TOC pointer: {toc_location}"));
    }
    let toc_offset =
        usize::try_from(toc_location).map_err(|_| "TOC pointer overflows".to_string())?;
    let mut offset = toc_offset;
    if offset >= bytes.len() {
        return Err(format!("TOC pointer {offset} is outside file"));
    }

    let _toc_marker = metakit_read_bp_int(bytes, &mut offset)?;
    let structure = metakit_read_p_string(bytes, &mut offset)?;
    let structure_len = structure.len();

    let reader = crate::metakit::MetakitReader::from_bytes(bytes)
        .map_err(|err| format!("MetakitReader failed: {err}"))?;
    let mut tables = Vec::with_capacity(reader.table_count());
    let mut sample_rows = Vec::new();
    let mut string_rows = Vec::new();
    let mut file_rows = Vec::new();
    for table in reader.tables() {
        let name = table.name().to_string();
        let columns = table
            .columns()
            .iter()
            .map(|column| VolocityMetakitColumn {
                name: column.name().to_string(),
                type_string: column.type_string().to_string(),
            })
            .collect::<Vec<_>>();
        let rows = table
            .rows()
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| value.map(VolocityMetakitValue::from))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let first_row_values = rows.first().map_or_else(Vec::new, |row| {
            volocity_metakit_row_scalar_summary(row, &columns)
        });
        let scalar_values = if table.row_count() == 1 {
            first_row_values.clone()
        } else {
            Vec::new()
        };
        if name == "samplesViewR" {
            sample_rows = volocity_sample_rows_from_metakit(&rows, &columns);
        } else if name == "stringsViewR" {
            string_rows = volocity_string_rows_from_metakit(&rows, &columns);
        } else if name == "filesViewR" {
            file_rows = volocity_file_rows_from_metakit(&rows, &columns);
        }
        tables.push(VolocityMetakitTable {
            name,
            row_count: Some(table.row_count()),
            columns,
            scalar_values,
            first_row_values,
        });
    }

    Ok(Some(VolocityMetakitProbe {
        little_endian,
        footer_offset: footer_pointer,
        toc_offset,
        structure_len,
        tables,
        stack_candidates: volocity_stack_candidates(
            &sample_rows,
            &string_rows,
            &file_rows,
            data_dir,
        ),
    }))
}

fn parse_volocity_blind_layout(
    bytes: &[u8],
) -> Result<Option<(ImageMetadata, VolocityBlindLayout)>> {
    if !bytes.starts_with(VOLOCITY_BLIND_MAGIC) {
        return Ok(None);
    }
    if bytes.len() < VOLOCITY_BLIND_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset header is truncated".into(),
        ));
    }

    let version = volocity_u16(bytes, 16);
    let pixel_code = volocity_u16(bytes, 18);
    let size_x = volocity_u32(bytes, 20);
    let size_y = volocity_u32(bytes, 24);
    let size_z = volocity_u32(bytes, 28);
    let size_c = volocity_u32(bytes, 32);
    let size_t = volocity_u32(bytes, 36);
    let flags = volocity_u16(bytes, 40);
    let reserved = volocity_u16(bytes, 42);
    let data_offset = volocity_u32(bytes, 44) as usize;

    if version != 1 {
        return Err(BioFormatsError::Format(format!(
            "Volocity MVD2 blind subset version {version} is not supported"
        )));
    }
    if [size_x, size_y, size_z, size_c, size_t].contains(&0) {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset dimensions must be positive".into(),
        ));
    }
    if flags & !1 != 0 || reserved != 0 {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset reserved header bits must be zero".into(),
        ));
    }
    if data_offset < VOLOCITY_BLIND_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset data offset points into header".into(),
        ));
    }
    if data_offset > bytes.len() {
        return Err(BioFormatsError::Format(
            "Volocity MVD2 blind subset data offset is past end of file".into(),
        ));
    }

    let pixel_type = match pixel_code {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        other => {
            return Err(BioFormatsError::Format(format!(
                "Volocity MVD2 blind subset pixel type {other} is not supported"
            )))
        }
    };
    let plane_len = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 plane size overflows".into()))?;
    let image_count = size_z
        .checked_mul(size_c)
        .and_then(|n| n.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 image count overflows".into()))?;
    let payload_len = plane_len
        .checked_mul(image_count as usize)
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 payload size overflows".into()))?;
    let payload_end = data_offset
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 payload end overflows".into()))?;
    if payload_end != bytes.len() {
        return Err(BioFormatsError::Format(format!(
            "Volocity MVD2 blind subset payload length {} does not match declared size {payload_len}",
            bytes.len().saturating_sub(data_offset)
        )));
    }

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_little_endian: flags & 1 != 0,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "volocity_version_subset".into(),
        MetadataValue::String("BFVOLOCITYMVD2-blind-raw-v1".into()),
    );
    meta.series_metadata.insert(
        "Volocity blind pixel type code".into(),
        MetadataValue::Int(i64::from(pixel_code)),
    );
    meta.series_metadata.insert(
        "Volocity blind data offset".into(),
        MetadataValue::Int(data_offset as i64),
    );

    Ok(Some((
        meta,
        VolocityBlindLayout {
            data_offset,
            plane_len,
        },
    )))
}

// --- Native series construction ----------------------------------------------
//
// Below this point we port the Java VolocityReader.initFile series-construction
// (Java lines ~481-691) and openBytes (Java lines ~142-219). The Metakit object
// graph parsed above is the equivalent of Java's `sampleTable`/`stringTable`;
// `VolocityStackCandidate` plays the role of the (parentIDs, stackNames) lists.
//
// One difference: Java keeps the parsed Metakit tables and re-resolves files via
// a live `Location` mapping. Here we resolve every companion file to an absolute
// path up-front during construction and read planes straight off disk.

#[allow(dead_code)]
const VOLOCITY_SIGNATURE_SIZE: usize = 13;
#[allow(dead_code)]
const VOLOCITY_EMBEDDED_STREAM: &str = "embedded-stream.raw";

/// Runtime equivalent of the Java `Stack` helper class plus the per-series
/// `CoreMetadata` it carries.
#[derive(Debug, Clone)]
struct VolocityStack {
    /// One entry per channel; either an absolute path to a companion file
    /// (`.aisf`/`.dat`) or the special `EMBEDDED_STREAM` marker.
    pixels_files: Vec<VolocityPixels>,
    timestamp_file: Option<PathBuf>,
    plane_padding: usize,
    block_size: usize,
    clipping_data: bool,

    channel_names: Vec<String>,
    name: String,
    description: Option<String>,

    // Core metadata for this series.
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    image_count: u32,
    pixel_type: PixelType,
    rgb: bool,
    little_endian: bool,
}

/// A single channel's pixel source: either an on-disk file or an inline
/// (embedded) byte buffer mapped from the Metakit sample data.
#[derive(Debug, Clone)]
enum VolocityPixels {
    File(PathBuf),
    Embedded(Vec<u8>),
}

impl VolocityPixels {
    fn exists(&self) -> bool {
        match self {
            VolocityPixels::File(path) => path.exists(),
            VolocityPixels::Embedded(_) => true,
        }
    }

    fn read_all(&self) -> Result<Vec<u8>> {
        match self {
            VolocityPixels::File(path) => std::fs::read(path).map_err(BioFormatsError::Io),
            VolocityPixels::Embedded(bytes) => Ok(bytes.clone()),
        }
    }

    fn is_aisf(&self) -> bool {
        matches!(self, VolocityPixels::File(path) if ext_lower(path).as_deref() == Some("aisf"))
    }
}

/// Java FormatTools.pixelTypeFromBytes.
fn volocity_pixel_type_from_bytes(bytes: i32, signed: bool, fp: bool) -> Result<PixelType> {
    Ok(match bytes {
        1 => {
            if signed {
                PixelType::Int8
            } else {
                PixelType::Uint8
            }
        }
        2 => {
            if signed {
                PixelType::Int16
            } else {
                PixelType::Uint16
            }
        }
        4 => {
            if fp {
                PixelType::Float32
            } else if signed {
                PixelType::Int32
            } else {
                PixelType::Uint32
            }
        }
        8 => PixelType::Float64,
        other => {
            return Err(BioFormatsError::Format(format!(
                "Volocity: unsupported byte depth {other}"
            )))
        }
    })
}

/// Java DataTools.swap(int) — byte-reverse a 32-bit value.
fn volocity_swap_i32(value: i32) -> i32 {
    i32::from_be_bytes(value.to_le_bytes())
}

/// Java CoreMetadata.getEffectiveSizeC: imageCount / (sizeZ * sizeT).
fn volocity_effective_size_c(stack: &VolocityStack) -> u32 {
    let size_zt = stack.size_z * stack.size_t;
    if size_zt == 0 {
        0
    } else {
        stack.image_count / size_zt
    }
}

/// Java CoreMetadata.getRGBChannelCount: sizeC / effectiveSizeC.
fn volocity_rgb_channel_count(stack: &VolocityStack) -> u32 {
    let eff = volocity_effective_size_c(stack);
    if eff == 0 {
        0
    } else {
        stack.size_c / eff
    }
}

/// Java FormatTools.getZCTCoords for dimensionOrder "XYCZT".
fn volocity_zct_coords(stack: &VolocityStack, no: u32) -> [u32; 3] {
    // dimensionOrder is always "XYCZT"; rasterize accordingly.
    let num_c = volocity_effective_size_c(stack).max(1);
    let num_z = stack.size_z.max(1);
    let c = no % num_c;
    let z = (no / num_c) % num_z;
    let t = no / (num_c * num_z);
    [z, c, t]
}

/// Java FormatTools.getPlaneSize: sizeX * sizeY * rgbChannelCount * bytesPerPixel.
fn volocity_plane_size(stack: &VolocityStack) -> usize {
    (stack.size_x as usize)
        * (stack.size_y as usize)
        * volocity_rgb_channel_count(stack) as usize
        * stack.pixel_type.bytes_per_sample()
}

/// Resolve a companion file id to an absolute path, trying both the raw id and
/// the byte-swapped id (Java initFile tries `DataTools.swap` as a fallback).
fn volocity_companion_path(data_dir: &Path, id: i32, ext: &str) -> PathBuf {
    let direct = data_dir.join(format!("{id}.{ext}"));
    if direct.exists() {
        return direct;
    }
    let swapped = data_dir.join(format!("{}.{ext}", volocity_swap_i32(id)));
    if swapped.exists() {
        return swapped;
    }
    direct
}

/// Build the runtime stacks from the parsed Metakit graph. This ports the body
/// of Java `initFile` from the parentIDs loop (line ~324) through the series
/// CoreMetadata loop (line ~691). Returns the stacks ready for `open_bytes`.
fn volocity_build_stacks(
    data_dir: &Path,
    candidates: &[VolocityStackCandidate],
) -> Result<Vec<VolocityStack>> {
    let mut stacks: Vec<VolocityStack> = Vec::new();

    for candidate in candidates {
        // pixelsFiles resolution (Java lines 329-380).
        let mut pixels_files: Vec<VolocityPixels> = Vec::new();
        let mut channel_names: Vec<String> = Vec::new();

        if candidate.channel_child_sample_id.is_some() && !candidate.channel_links.is_empty() {
            for channel in &candidate.channel_links {
                channel_names.push(channel.name.clone());
                if let Some(aisf_id) = channel.aisf_id {
                    pixels_files.push(VolocityPixels::File(volocity_companion_path(
                        data_dir, aisf_id, "aisf",
                    )));
                } else if let Some(dat) = &channel.pixels_dat {
                    pixels_files.push(VolocityPixels::File(data_dir.join(dat)));
                } else {
                    // No usable pixels source for this channel; skip the stack.
                    pixels_files.push(VolocityPixels::File(data_dir.join("__missing__")));
                }
            }
        } else {
            // Java initFile non-channel branch (lines 360-379): pixelsFiles[0] =
            // getFile(parent, dir); if that file is null or does not exist, fall
            // back to the stack sample's own inline data mapped as the embedded
            // stream.
            let dat_path = candidate.pixels_dat.as_ref().map(|dat| data_dir.join(dat));
            match dat_path {
                Some(path) if path.exists() => pixels_files.push(VolocityPixels::File(path)),
                _ => match &candidate.inline_data {
                    Some(bytes) => pixels_files.push(VolocityPixels::Embedded(bytes.clone())),
                    // No .dat and no inline data: keep the (missing) .dat path so
                    // the later existence filter drops the stack, matching Java
                    // which would NPE here only if sampleData were absent.
                    None => pixels_files.push(VolocityPixels::File(
                        candidate
                            .pixels_dat
                            .as_ref()
                            .map(|dat| data_dir.join(dat))
                            .unwrap_or_else(|| data_dir.join("__missing__")),
                    )),
                },
            }
        }

        if pixels_files.is_empty() {
            continue;
        }

        let timestamp_file = candidate
            .metadata
            .timestamp_atsf_id
            .map(|id| volocity_companion_path(data_dir, id, "atsf"));

        let mut stack = VolocityStack {
            pixels_files,
            timestamp_file,
            plane_padding: 0,
            block_size: 0,
            clipping_data: false,
            channel_names: if channel_names.is_empty() {
                Vec::new()
            } else {
                channel_names
            },
            name: candidate.stack_name.clone(),
            description: candidate.metadata.description.clone(),
            size_x: 0,
            size_y: 0,
            size_z: 1,
            size_c: candidate
                .channel_count
                .map(|c| c.max(1) as u32)
                .unwrap_or(1),
            size_t: 1,
            image_count: 0,
            pixel_type: PixelType::Uint8,
            rgb: false,
            little_endian: true,
        };
        // If we have channel files, sizeC follows the channel count.
        if !stack.channel_names.is_empty() {
            stack.size_c = stack.channel_names.len() as u32;
        }
        stacks.push(stack);
    }

    // Channel-split (Java lines 481-540): a channel file longer than the base
    // file indicates a separate stack starts there.
    volocity_split_channels(&mut stacks);

    // Drop stacks whose base pixels file does not exist (Java line 484-488).
    stacks.retain(|stack| {
        stack
            .pixels_files
            .first()
            .is_some_and(VolocityPixels::exists)
    });

    // Per-series CoreMetadata (Java lines 544-691).
    let mut result = Vec::with_capacity(stacks.len());
    for mut stack in stacks {
        if volocity_init_core_metadata(&mut stack).is_ok() {
            result.push(stack);
        }
    }

    Ok(result)
}

/// Java initFile channel-split loop (lines 481-540).
fn volocity_split_channels(stacks: &mut Vec<VolocityStack>) {
    let mut i = 0;
    while i < stacks.len() {
        if !stacks[i]
            .pixels_files
            .first()
            .is_some_and(VolocityPixels::exists)
        {
            stacks.remove(i);
            continue;
        }
        let base_length = stacks[i]
            .pixels_files
            .first()
            .and_then(|p| match p {
                VolocityPixels::File(path) => std::fs::metadata(path).ok().map(|m| m.len()),
                VolocityPixels::Embedded(bytes) => Some(bytes.len() as u64),
            })
            .unwrap_or(0);

        let mut q = 1;
        while q < stacks[i].pixels_files.len() {
            let pix = &stacks[i].pixels_files[q];
            if !pix.exists() {
                q += 1;
                continue;
            }
            let length = match pix {
                VolocityPixels::File(path) => {
                    std::fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0)
                }
                VolocityPixels::Embedded(bytes) => bytes.len() as u64,
            };
            if length > base_length {
                // Split: everything from q onwards becomes a new stack.
                let src = &stacks[i];
                let tail_pixels = src.pixels_files[q..].to_vec();
                let head_pixels = src.pixels_files[..q].to_vec();
                let tail_channels: Vec<String> = if src.channel_names.len() > q {
                    src.channel_names[q..].to_vec()
                } else {
                    Vec::new()
                };
                let head_channels: Vec<String> = if src.channel_names.len() >= q {
                    src.channel_names[..q].to_vec()
                } else {
                    src.channel_names.clone()
                };

                let mut new_stack = src.clone();
                new_stack.pixels_files = tail_pixels;
                new_stack.channel_names = tail_channels.clone();
                new_stack.size_c = tail_channels.len().max(1) as u32;

                stacks[i].pixels_files = head_pixels;
                stacks[i].channel_names = head_channels.clone();
                stacks[i].size_c = head_channels.len().max(1) as u32;

                stacks.insert(i + 1, new_stack);
                break;
            }
            q += 1;
        }
        i += 1;
    }
}

/// Java initFile per-series CoreMetadata block (lines 544-691). Reads the
/// timestamp file (if any) for sizeT, then derives X/Y/Z/C/pixelType from the
/// base pixels file header. Returns Err if the base file cannot be read.
fn volocity_init_core_metadata(stack: &mut VolocityStack) -> Result<()> {
    stack.little_endian = true;
    stack.rgb = false;

    // Timestamp file → sizeT (Java lines 553-579).
    if let Some(ts) = &stack.timestamp_file {
        if let Ok(bytes) = std::fs::read(ts) {
            if bytes.first().copied() != Some(b'I') {
                stack.little_endian = false;
            }
            let size_t = volocity_read_stream_i32(&bytes, 17, stack.little_endian).unwrap_or(0);
            stack.size_t = size_t.max(0) as u32;
        } else {
            stack.size_t = 1;
        }
    } else {
        stack.size_t = 1;
    }
    if stack.size_t == 0 {
        stack.size_t = 1;
    }

    let base = stack
        .pixels_files
        .first()
        .ok_or(BioFormatsError::NotInitialized)?;
    let is_aisf = base.is_aisf();
    let embedded = matches!(base, VolocityPixels::Embedded(_));
    let bytes = base.read_all()?;
    let file_len = bytes.len() as i64;

    if is_aisf {
        volocity_init_core_aisf(stack, &bytes, file_len)?;
    } else {
        volocity_init_core_native(stack, &bytes, file_len, embedded)?;
    }

    Ok(())
}

/// Java initFile `.aisf` branch (lines 589-642).
fn volocity_init_core_aisf(stack: &mut VolocityStack, bytes: &[u8], file_len: i64) -> Result<()> {
    let mut le = stack.little_endian;
    let block_size_raw = volocity_read_stream_i16(bytes, 18, le).unwrap_or(0);
    stack.block_size = (block_size_raw as i32 * 256).max(0) as usize;

    // After short at 18 (2 bytes) + skip 5 → offset 25 begins the int run.
    let mut off = 25usize;
    let mut x = volocity_read_stream_i32(bytes, off, le).unwrap_or(0);
    let mut y = volocity_read_stream_i32(bytes, off + 4, le).unwrap_or(0);
    let mut z_start = volocity_read_stream_i32(bytes, off + 8, le).unwrap_or(0);
    let mut w = volocity_read_stream_i32(bytes, off + 12, le).unwrap_or(0);
    let mut h = volocity_read_stream_i32(bytes, off + 16, le).unwrap_or(0);

    if w - x < 0 || h - y < 0 || (w - x).wrapping_mul(h - y) < 0 {
        le = !le;
        stack.little_endian = le;
        x = volocity_read_stream_i32(bytes, off, le).unwrap_or(0);
        y = volocity_read_stream_i32(bytes, off + 4, le).unwrap_or(0);
        z_start = volocity_read_stream_i32(bytes, off + 8, le).unwrap_or(0);
        w = volocity_read_stream_i32(bytes, off + 12, le).unwrap_or(0);
        h = volocity_read_stream_i32(bytes, off + 16, le).unwrap_or(0);
    }
    off += 20;

    stack.size_x = (w - x).max(0) as u32;
    stack.size_y = (h - y).max(0) as u32;
    let z_end = volocity_read_stream_i32(bytes, off, le).unwrap_or(0);
    stack.size_z = (z_end - z_start).max(0) as u32;
    stack.pixel_type = PixelType::Int8;
    stack.image_count = stack.size_z * stack.size_c * stack.size_t;

    if stack.size_x == 0 || stack.size_y == 0 || stack.size_z == 0 {
        return Err(BioFormatsError::Format(
            "Volocity: empty .aisf dimensions".into(),
        ));
    }

    let planes_per_file = (stack.size_z * stack.size_t).max(1) as i64;
    let plane_size = volocity_plane_size(stack) as i64;
    if plane_size <= 0 {
        return Err(BioFormatsError::Format("Volocity: zero plane size".into()));
    }
    let mut bytes_per_plane = (file_len - stack.block_size as i64) / planes_per_file;
    let mut bytes_per_pixel: i32 = 0;
    while bytes_per_plane >= plane_size {
        bytes_per_pixel += 1;
        bytes_per_plane -= plane_size;
    }

    // Java: `if ((bytesPerPixel % 3) == 0)` — note bytesPerPixel can be 0 here,
    // and 0 % 3 == 0, so Java would multiply sizeC by 3 and divide 0/3 = 0. We
    // mirror that exactly (the subsequent pixelTypeFromBytes(0,..) would then
    // throw in Java; we keep the bytes_per_pixel value as-is for fidelity and
    // let volocity_pixel_type_from_bytes surface the same failure).
    if (bytes_per_pixel % 3) == 0 {
        stack.size_c *= 3;
        stack.rgb = true;
        bytes_per_pixel /= 3;
    }
    stack.pixel_type = volocity_pixel_type_from_bytes(bytes_per_pixel, false, bytes_per_pixel > 2)?;
    // NB: Java does NOT recompute imageCount here; it keeps the value from line
    // 613 (sizeZ * original sizeC * sizeT), which makes effectiveSizeC == the
    // original channel count and rgbChannelCount == 3 for RGB stacks.

    // Per-timepoint padding to a multiple of blockSize (Java lines 636-641).
    if stack.block_size > 0 {
        let timepoint = volocity_plane_size(stack) * stack.size_z as usize;
        let mut padding = stack.block_size - (timepoint % stack.block_size);
        if padding == stack.block_size {
            padding = 0;
        }
        stack.plane_padding = padding;
    }
    Ok(())
}

/// Java initFile native (`.dat`/embedded) branch (lines 643-689).
fn volocity_init_core_native(
    stack: &mut VolocityStack,
    bytes: &[u8],
    file_len: i64,
    embedded: bool,
) -> Result<()> {
    let mut le = bytes.first().copied() == Some(b'I');
    stack.little_endian = le;
    if !le {
        le = false;
    }

    stack.size_x = volocity_read_stream_i32(bytes, 22, le).unwrap_or(0).max(0) as u32;
    stack.size_y = volocity_read_stream_i32(bytes, 26, le).unwrap_or(0).max(0) as u32;
    stack.size_z = volocity_read_stream_i32(bytes, 30, le).unwrap_or(0).max(0) as u32;
    stack.size_c = if embedded { 1 } else { 4 };
    stack.image_count = stack.size_z * stack.size_t;
    stack.rgb = stack.size_c > 1;
    stack.pixel_type = PixelType::Uint8;
    // blockSize: embedded uses current file pointer (offset 34 after 3 ints),
    // otherwise the magic constant 99 (Java line 660).
    stack.block_size = if embedded { 34 } else { 99 };
    stack.plane_padding = 0;

    let px = stack.size_x as i64 * stack.size_y as i64 * stack.size_z as i64;

    if file_len > px * 6 {
        stack.pixel_type = PixelType::Uint16;
        stack.size_c = 3;
        stack.rgb = true;
    }

    if file_len < px * stack.size_c as i64 {
        stack.rgb = false;
        stack.size_c = 1;
        let pixels = px.max(1);
        let approx = file_len as f64 / pixels as f64;
        let mut nbytes = approx.ceil() as i32;
        if nbytes == 0 {
            nbytes = 1;
        } else if nbytes == 3 {
            nbytes = 2;
        }
        stack.pixel_type = volocity_pixel_type_from_bytes(nbytes, false, false)?;
        stack.block_size = volocity_read_stream_i32(bytes, 70, le).unwrap_or(0).max(0) as usize;
        stack.clipping_data = true;
    }

    stack.image_count = stack.size_z * stack.size_t;
    if stack.size_x == 0 || stack.size_y == 0 || stack.size_z == 0 {
        return Err(BioFormatsError::Format(
            "Volocity: empty native stream dimensions".into(),
        ));
    }
    Ok(())
}

fn volocity_read_stream_i16(bytes: &[u8], offset: usize, little_endian: bool) -> Option<i16> {
    let data = bytes.get(offset..offset.checked_add(2)?)?;
    Some(if little_endian {
        i16::from_le_bytes([data[0], data[1]])
    } else {
        i16::from_be_bytes([data[0], data[1]])
    })
}

/// Build the `ImageMetadata` view of a stack (used by `metadata()`).
fn volocity_stack_metadata(stack: &VolocityStack) -> ImageMetadata {
    let mut meta = ImageMetadata {
        size_x: stack.size_x,
        size_y: stack.size_y,
        size_z: stack.size_z,
        size_c: stack.size_c,
        size_t: stack.size_t,
        pixel_type: stack.pixel_type,
        bits_per_pixel: (stack.pixel_type.bytes_per_sample() * 8) as u8,
        image_count: stack.image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: stack.rgb,
        is_interleaved: true,
        is_little_endian: stack.little_endian,
        ..ImageMetadata::default()
    };
    meta.series_metadata
        .insert("Name".into(), MetadataValue::String(stack.name.clone()));
    if let Some(description) = &stack.description {
        meta.series_metadata.insert(
            "Description".into(),
            MetadataValue::String(description.clone()),
        );
    }
    for channel in &stack.channel_names {
        meta.series_metadata.insert(
            format!("Channel {}", channel),
            MetadataValue::String(channel.clone()),
        );
    }
    meta
}

/// Java openBytes (lines 142-219) for a single plane of a stack.
fn volocity_open_plane(stack: &VolocityStack, no: u32) -> Result<Vec<u8>> {
    let zct = volocity_zct_coords(stack, no);
    let plane_size = volocity_plane_size(stack);
    let mut buf = vec![0u8; plane_size];

    let channel = zct[1] as usize;
    let pixels = stack
        .pixels_files
        .get(channel)
        .ok_or(BioFormatsError::PlaneOutOfRange(no))?;
    if !pixels.exists() {
        // Java fills with the fill color (0 by default).
        return Ok(buf);
    }
    let data = pixels.read_all()?;
    let pix_len = data.len() as i64;

    let mut padding = (zct[2] as usize) * stack.plane_padding;
    let planes_in_file = if plane_size > 0 {
        (pix_len / plane_size as i64) as i64
    } else {
        0
    };
    let mut plane_index = (no / volocity_effective_size_c(stack).max(1)) as i64;
    if planes_in_file == stack.size_t as i64 {
        plane_index = zct[2] as i64;
        let block = stack.block_size as i64;
        if block > 0 {
            let mut pad = block - (plane_size as i64 % block);
            if pad == block {
                pad = 0;
            }
            padding = (pad * zct[2] as i64) as usize;
        }
    }

    let offset = stack.block_size as i64 + plane_index * plane_size as i64 + padding as i64;
    if offset >= pix_len || offset < 0 {
        return Ok(buf);
    }

    if stack.clipping_data {
        volocity_read_clipping_plane(stack, &data, offset, &mut buf)?;
    } else {
        let start = offset as usize;
        let end = start.saturating_add(plane_size);
        if end > data.len() {
            return Ok(buf);
        }
        buf.copy_from_slice(&data[start..end]);
    }

    // RGBA swap (Java lines 207-216): stored ARGB → RGBA.
    if volocity_rgb_channel_count(stack) == 4 {
        for chunk in buf.chunks_exact_mut(4) {
            let a = chunk[0];
            chunk[0] = chunk[1];
            chunk[1] = chunk[2];
            chunk[2] = chunk[3];
            chunk[3] = a;
        }
    }

    Ok(buf)
}

/// Java openBytes clipping (LZO) branch (lines 181-197).
///
/// Java seeks to `offset - 3` and repeatedly calls
/// `new LZOCodec().decompress(pix, null)` followed by `pix.skipBytes(4)`,
/// appending each decoded block to a `ByteArrayHandle` until a full plane has
/// been produced (or EOF). The `ome.codecs` LZOCodec reads a raw LZO1X stream
/// directly from the input — there is NO length prefix — and stops at the LZO
/// end marker, leaving the stream positioned right after the block; the
/// trailing 4 bytes are then skipped.
///
/// We use `decompress_lzo_with_consumed`, which reports the number of input
/// bytes consumed by each raw LZO1X block, so we can replicate Java's per-block
/// loop: decode a block, skip 4 trailing bytes, repeat until a full plane has
/// been produced (or the input is exhausted). Java also breaks on the first
/// failing block by catching the IOException without advancing; we mirror that
/// by stopping the loop as soon as a block fails to decode.
fn volocity_read_clipping_plane(
    stack: &VolocityStack,
    data: &[u8],
    offset: i64,
    buf: &mut [u8],
) -> Result<()> {
    let plane_size = volocity_plane_size(stack);
    let mut pos = (offset - 3).max(0) as usize;
    let mut out: Vec<u8> = Vec::with_capacity(plane_size);

    // Java: while (v.length() < planeSize && pix.getFilePointer() < pix.length())
    while out.len() < plane_size && pos < data.len() {
        // Feed the raw LZO1X block starting here (no length prefix).
        match crate::common::codec::decompress_lzo_with_consumed(&data[pos..]) {
            Ok((decoded, consumed)) => {
                out.extend_from_slice(&decoded);
                // Java: pix.skipBytes(4) after each decoded block.
                pos = pos.saturating_add(consumed).saturating_add(4);
                // Guard against a zero-length block that does not advance the
                // input pointer (Java relies on the stream advancing too).
                if consumed == 0 {
                    break;
                }
            }
            // Java catches the IOException and re-enters the loop, but since the
            // stream pointer is unchanged it would spin; the decode failing on a
            // partial block effectively terminates plane production.
            Err(_) => break,
        }
    }

    let copy_len = out.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&out[..copy_len]);
    Ok(())
}

pub struct VolocityReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    bytes: Vec<u8>,
    layout: Option<VolocityBlindLayout>,
    /// Native Metakit-backed series. When non-empty, the reader serves real
    /// Volocity data; `meta`/`layout` are only used for the blind-raw subset.
    stacks: Vec<VolocityStack>,
    series_meta: Vec<ImageMetadata>,
    current_series: usize,
}

impl VolocityReader {
    pub fn new() -> Self {
        VolocityReader {
            path: None,
            meta: None,
            bytes: Vec::new(),
            layout: None,
            stacks: Vec::new(),
            series_meta: Vec::new(),
            current_series: 0,
        }
    }
}
impl Default for VolocityReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VolocityReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = ext_lower(path);
        match ext.as_deref() {
            Some("mvd2") => true,
            suffix if is_volocity_companion_suffix(suffix) => {
                volocity_library_from_companion(path).is_some()
            }
            _ => false,
        }
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.starts_with(VOLOCITY_BLIND_MAGIC) {
            return true;
        }
        // Java `VolocityReader.isThisType(RandomAccessInputStream)` accepts the
        // two-byte Metakit signature directly.
        matches!(header.get(0..2), Some(b"JL") | Some(b"LJ"))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let root = if ext_lower(path).as_deref() == Some("mvd2") {
            path.to_path_buf()
        } else {
            volocity_library_from_companion_for_init(path).unwrap_or_else(|| path.to_path_buf())
        };
        self.path = None;
        self.meta = None;
        self.bytes.clear();
        self.layout = None;
        self.stacks.clear();
        self.series_meta.clear();
        self.current_series = 0;
        if root.exists() {
            let bytes = std::fs::read(&root).map_err(BioFormatsError::Io)?;
            if let Some((meta, layout)) = parse_volocity_blind_layout(&bytes)? {
                self.path = Some(root);
                self.meta = Some(meta);
                self.bytes = bytes;
                self.layout = Some(layout);
                return Ok(());
            }
            // Java initFile resolves companion pixel files relative to a sibling
            // "Data" directory next to the .mvd2 library file. This same dir is
            // threaded into the probe so getStream() can open external `.dat`
            // streams for candidate gating, channel `.aisf` ids and metadata.
            let data_dir = root
                .parent()
                .map(|parent| parent.join(DATA_DIR))
                .unwrap_or_else(|| PathBuf::from(DATA_DIR));
            match probe_volocity_metakit(&bytes, Some(&data_dir)) {
                Ok(Some(probe)) => {
                    if data_dir.is_dir() {
                        if let Ok(stacks) =
                            volocity_build_stacks(&data_dir, &probe.stack_candidates)
                        {
                            if !stacks.is_empty() {
                                self.series_meta =
                                    stacks.iter().map(volocity_stack_metadata).collect();
                                self.stacks = stacks;
                                self.path = Some(root);
                                return Ok(());
                            }
                        }
                    }
                    // No usable companion data → fall back to the diagnostic.
                    return Err(volocity_native_error(&root, &probe));
                }
                Ok(None) => {}
                Err(reason) if bytes.starts_with(b"JL") || bytes.starts_with(b"LJ") => {
                    return Err(volocity_metakit_probe_error(&root, &reason));
                }
                Err(_) => {}
            }
        }
        Err(volocity_error(Some(&root)))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.bytes.clear();
        self.layout = None;
        self.stacks.clear();
        self.series_meta.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        if !self.stacks.is_empty() {
            self.stacks.len()
        } else {
            usize::from(self.meta.is_some())
        }
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if !self.stacks.is_empty() {
            if s < self.stacks.len() {
                self.current_series = s;
                Ok(())
            } else {
                Err(BioFormatsError::SeriesOutOfRange(s))
            }
        } else if s == 0 && self.meta.is_some() {
            Ok(())
        } else if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }
    fn series(&self) -> usize {
        if self.stacks.is_empty() {
            0
        } else {
            self.current_series
        }
    }
    fn metadata(&self) -> &ImageMetadata {
        if !self.series_meta.is_empty() {
            return self
                .series_meta
                .get(self.current_series)
                .unwrap_or(crate::common::reader::uninitialized_metadata());
        }
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if !self.stacks.is_empty() {
            let stack = self
                .stacks
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            if p >= stack.image_count {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            return volocity_open_plane(stack, p);
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let layout = self.layout.ok_or(BioFormatsError::NotInitialized)?;
        let start = layout
            .data_offset
            .checked_add(layout.plane_len * p as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("Volocity MVD2 plane offset overflows".into())
            })?;
        let end = start
            .checked_add(layout.plane_len)
            .ok_or_else(|| BioFormatsError::Format("Volocity MVD2 plane end overflows".into()))?;
        Ok(self.bytes[start..end].to_vec())
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        if !self.stacks.is_empty() {
            let stack = self
                .stacks
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            return crop_full_plane(
                "Volocity MVD2",
                &full,
                self.metadata(),
                volocity_rgb_channel_count(stack).max(1) as usize,
                x,
                y,
                w,
                h,
            );
        }
        let meta = self.metadata();
        let rgb = if meta.is_rgb { meta.size_c.max(1) } else { 1 };
        crop_full_plane("Volocity MVD2", &full, meta, rgb as usize, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.open_bytes(p)
    }
}

// --- Nikon NIS-Elements .nif --------------------------------------------------
//
// Nikon NIS-Elements Image File (.nif) — TIFF-based format.
// Delegates to TiffReader for pixel data.

pub struct NikonNisReader {
    inner: crate::tiff::TiffReader,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_volocity_{nanos}_{name}"))
    }

    #[test]
    fn volocity_matches_java_stream_signature() {
        let reader = VolocityReader::new();
        assert!(reader.is_this_type_by_bytes(VOLOCITY_BLIND_MAGIC));
        assert!(reader.is_this_type_by_bytes(b"JLabcdef"));
        assert!(reader.is_this_type_by_bytes(b"LJabcdef"));
        assert!(!reader.is_this_type_by_bytes(b"JXabcdef"));
        assert!(!reader.is_this_type_by_bytes(b"J"));
    }

    #[test]
    fn volocity_detects_bounded_native_metakit_stream() {
        let bytes = include_bytes!("../metakit/tests/data/test.mk");
        let reader = VolocityReader::new();
        assert!(reader.is_this_type_by_bytes(bytes));

        let probe = probe_volocity_metakit(bytes, None).unwrap().unwrap();
        assert!(probe.little_endian);
        assert_eq!(probe.footer_offset, 22569);
        assert_eq!(probe.toc_offset, 1496);
        assert_eq!(probe.structure_len, 488);
        assert_eq!(
            probe
                .tables
                .iter()
                .map(|table| (table.name.as_str(), table.row_count))
                .collect::<Vec<_>>(),
            vec![
                ("variablesView", Some(1)),
                ("samplesViewR", Some(29)),
                ("stringsViewR", Some(23)),
                ("filesViewR", Some(0)),
            ]
        );
        assert_eq!(
            probe.tables[0]
                .columns
                .iter()
                .map(|column| (column.name.as_str(), column.type_string.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("varVersion", "I"),
                ("varNextSampleID", "I"),
                ("varNextStringID", "I"),
                ("varNextFileID", "I"),
                ("varDemoKey", "I"),
            ]
        );
        assert_eq!(
            probe.tables[0]
                .scalar_values
                .iter()
                .map(|(column, value)| (column.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("varVersion", "2"),
                ("varNextSampleID", "30"),
                ("varNextStringID", "24"),
                ("varNextFileID", "2"),
            ]
        );
        assert_eq!(
            probe.tables[1]
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "sampleID",
                "sampleParent",
                "sampleChildType",
                "sampleChildPos",
                "sampleOrigChildPos",
                "sampleIsLinked",
                "sampleIsCloaked",
                "sampleIsDeleted",
                "sampleDataVersion",
                "sampleDataRevision",
                "sampleFileLink",
                "sampleNameLink",
                "sampleChangeTime",
                "sampleData",
                "sampleExternalData",
            ]
        );
        assert_eq!(
            probe.tables[1]
                .first_row_values
                .iter()
                .map(|(column, value)| (column.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("sampleID", "1"),
                ("sampleParent", "0"),
                ("sampleChildType", "1"),
                ("sampleChildPos", "1"),
                ("sampleOrigChildPos", "1"),
                ("sampleDataVersion", "503"),
                ("sampleDataRevision", "0"),
                ("sampleNameLink", "1"),
                ("sampleChangeTime", "3391447735797000"),
                (
                    "sampleData",
                    "21 bytes hex=493100020058020000f501020064000000f7010000",
                ),
                ("sampleExternalData", "0"),
            ]
        );
        assert_eq!(
            probe.tables[2]
                .first_row_values
                .iter()
                .map(|(column, value)| (column.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("stringID", "1"),
                ("stringString", "\"clipping-test.mvd2\\0\""),
                ("stringRefCount", "1"),
            ]
        );
        assert_eq!(
            probe.stack_candidates,
            vec![VolocityStackCandidate {
                sample_id: 5,
                stack_name: "Tx red 2".to_string(),
                parent_id: 1,
                name_link: Some(5),
                file_link: None,
                resolved_file: None,
                // getFile(parent=5) reads sampleExternalData (col 14) = 1 → "1.dat".
                pixels_dat: Some("1.dat".to_string()),
                channel_child_sample_id: None,
                channel_count: None,
                channel_links: Vec::new(),
                inline_data_len: 0,
                inline_data: Some(Vec::new()),
                native_stream_clue: None,
                external_data: Some(1),
                metadata: VolocityStackMetadata {
                    timestamp_atsf_id: None,
                    physical_x: Some(0.10320283208969615),
                    physical_y: Some(0.10320283208969615),
                    physical_z: Some(1.0),
                    magnification: Some(20.0),
                    detector: Some("HAMAMATSU C4742-80-12AG".to_string()),
                    description: Some(String::new()),
                    x_location: None,
                    y_location: None,
                    z_location: None,
                },
            }]
        );
        assert_eq!(
            volocity_native_semantic_summary(&probe).as_deref(),
            Some(
                "variables=variablesView(1), samples=samplesViewR(29), strings=stringsViewR(23), files=filesViewR(0); variables version=2, next_sample_id=30, next_string_id=24, next_file_id=2; sample hierarchy id=sampleID, parent=sampleParent, child_type=sampleChildType, child_pos=sampleChildPos, original_child_pos=sampleOrigChildPos, file_link=sampleFileLink, name_link=sampleNameLink, inline_data=sampleData, external_data=sampleExternalData; first sample row sampleID=1, sampleParent=0, sampleChildType=1 (Java stack-candidate branch), sampleChildPos=1, sampleOrigChildPos=1, sampleDataVersion=503, sampleDataRevision=0, sampleNameLink=1, sampleChangeTime=3391447735797000, sampleData=21 bytes hex=493100020058020000f501020064000000f7010000, sampleExternalData=0; string links stringsViewR[stringID, stringString, stringRefCount]; first string links row stringID=1, stringString=\"clipping-test.mvd2\\0\", stringRefCount=1; file links filesViewR[fileID, fileName, fileSpec, fileRefCount]; Java stack candidates 1: sampleID=5, name=\"Tx red 2\", parent=1, inline_data=0B, name_link=5, pixels_dat=1.dat, external_data=1, physicalX=0.10320283208969615, physicalY=0.10320283208969615, physicalZ=1, magnification=20, detector=\"HAMAMATSU C4742-80-12AG\", description=\"\""
            )
        );
    }

    fn volocity_test_sample(
        id: i32,
        parent: i32,
        child_type: i32,
        name_link: i32,
        inline_data: Option<Vec<u8>>,
        external_data: Option<i32>,
    ) -> VolocitySampleRow {
        VolocitySampleRow {
            id,
            parent,
            child_type,
            file_link: None,
            name_link: Some(name_link),
            inline_data_len: inline_data.as_ref().map_or(0, Vec::len),
            inline_data,
            external_data,
        }
    }

    fn volocity_test_inline_stream(width: i32, height: i32, depth: i32) -> Vec<u8> {
        let mut bytes = vec![0; 96];
        bytes[0] = b'I';
        bytes[22..26].copy_from_slice(&width.to_le_bytes());
        bytes[26..30].copy_from_slice(&height.to_le_bytes());
        bytes[30..34].copy_from_slice(&depth.to_le_bytes());
        bytes
    }

    fn volocity_test_channel_stream(aisf_id: i32) -> Vec<u8> {
        let mut bytes = vec![0; 32];
        bytes[22..26].copy_from_slice(&aisf_id.to_le_bytes());
        bytes
    }

    #[test]
    fn volocity_stack_candidates_validate_inline_stream_and_channels() {
        let strings = vec![
            VolocityStringRow {
                id: 1,
                value: "Library".to_string(),
            },
            VolocityStringRow {
                id: 2,
                value: "Invalid inline".to_string(),
            },
            VolocityStringRow {
                id: 3,
                value: "Inline stack".to_string(),
            },
            VolocityStringRow {
                id: 4,
                value: "Channel stack".to_string(),
            },
            VolocityStringRow {
                id: 5,
                value: "Channels".to_string(),
            },
            VolocityStringRow {
                id: 6,
                value: "DAPI".to_string(),
            },
            VolocityStringRow {
                id: 7,
                value: "FITC".to_string(),
            },
        ];
        let samples = vec![
            volocity_test_sample(1, 0, 1, 1, None, None),
            volocity_test_sample(2, 1, 1, 2, Some(vec![0; 96]), None),
            volocity_test_sample(3, 1, 1, 3, Some(volocity_test_inline_stream(4, 5, 2)), None),
            volocity_test_sample(4, 1, 1, 4, None, None),
            volocity_test_sample(5, 4, 0, 5, None, None),
            volocity_test_sample(6, 5, 0, 6, Some(volocity_test_channel_stream(101)), None),
            volocity_test_sample(7, 5, 0, 7, Some(volocity_test_channel_stream(202)), None),
        ];

        let candidates = volocity_stack_candidates(&samples, &strings, &[], None);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].sample_id, 3);
        assert_eq!(candidates[0].stack_name, "Inline stack");
        assert_eq!(
            candidates[0].native_stream_clue,
            Some(VolocityNativeStreamClue {
                little_endian: true,
                size_x: 4,
                size_y: 5,
                size_z: 2,
                stream_len: 96,
            })
        );
        assert_eq!(candidates[1].sample_id, 4);
        assert_eq!(candidates[1].channel_child_sample_id, Some(5));
        assert_eq!(candidates[1].channel_count, Some(2));
        assert_eq!(
            candidates[1].channel_links,
            vec![
                VolocityChannelLink {
                    sample_id: 6,
                    name: "DAPI".to_string(),
                    aisf_id: Some(101),
                    pixels_dat: None,
                },
                VolocityChannelLink {
                    sample_id: 7,
                    name: "FITC".to_string(),
                    aisf_id: Some(202),
                    pixels_dat: None,
                },
            ]
        );
        assert_eq!(candidates[1].native_stream_clue, None);

        let probe = VolocityMetakitProbe {
            little_endian: true,
            footer_offset: 0,
            toc_offset: 0,
            structure_len: 0,
            tables: Vec::new(),
            stack_candidates: candidates,
        };
        let summary = volocity_native_semantic_summary(&probe).unwrap();
        assert!(summary.contains("native_stream=4x5x2 LE len=96B"));
        assert!(summary.contains("channels_child=5, channels=2"));
        assert!(summary.contains(
            "channel_links=[sampleID=6 name=\"DAPI\" aisf_id=101, sampleID=7 name=\"FITC\" aisf_id=202]"
        ));
    }

    #[test]
    fn volocity_stack_candidates_resolve_sample_file_links() {
        let strings = vec![
            VolocityStringRow {
                id: 1,
                value: "Library".to_string(),
            },
            VolocityStringRow {
                id: 2,
                value: "File-backed stack".to_string(),
            },
        ];
        // Non-channel stacks are gated by getStream(i)'s x*y*z (Java line 311).
        // With data_dir=None the external `.dat` is unavailable, so we supply a
        // valid inline stream that getStream falls back to, exercising the file-
        // link resolution this test targets. external_data=Some(42) still drives
        // getFile()/pixels_dat below.
        let mut stack = volocity_test_sample(
            2,
            1,
            1,
            2,
            Some(volocity_test_inline_stream(4, 5, 2)),
            Some(42),
        );
        stack.file_link = Some(7);
        let samples = vec![volocity_test_sample(1, 0, 1, 1, None, None), stack];
        let files = vec![VolocityFileRow {
            id: 7,
            name: Some("plane-data.dat\0".to_string()),
            spec: Some(vec![0xde, 0xad, 0xbe, 0xef]),
        }];

        let candidates = volocity_stack_candidates(&samples, &strings, &files, None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].file_link, Some(7));
        assert_eq!(
            candidates[0].resolved_file,
            Some(VolocityFileLink {
                file_id: 7,
                name: Some("plane-data.dat".to_string()),
                spec_preview: Some("4 bytes hex=deadbeef".to_string()),
            })
        );
        // Java getFile(parent, dir) maps the stack's own sampleExternalData
        // (column 14) to "<value>.dat", NOT the sampleFileLink (column 10).
        assert_eq!(candidates[0].pixels_dat.as_deref(), Some("42.dat"));

        let probe = VolocityMetakitProbe {
            little_endian: true,
            footer_offset: 0,
            toc_offset: 0,
            structure_len: 0,
            tables: Vec::new(),
            stack_candidates: candidates,
        };
        let summary = volocity_native_semantic_summary(&probe).unwrap();
        assert!(summary.contains("file_link=7"));
        assert!(
            summary.contains("file=[fileID=7 name=\"plane-data.dat\" spec=4 bytes hex=deadbeef]")
        );
        assert!(summary.contains("pixels_dat=42.dat"));
    }

    #[test]
    fn volocity_stack_candidates_extract_named_child_metadata() {
        let strings = vec![
            VolocityStringRow {
                id: 1,
                value: "Library".to_string(),
            },
            VolocityStringRow {
                id: 2,
                value: "Metadata stack".to_string(),
            },
            VolocityStringRow {
                id: 3,
                value: "um/pixel (X)".to_string(),
            },
            VolocityStringRow {
                id: 4,
                value: "Microscope Objective".to_string(),
            },
            VolocityStringRow {
                id: 5,
                value: "Camera/Detector".to_string(),
            },
            VolocityStringRow {
                id: 6,
                value: "Timepoint times stream".to_string(),
            },
        ];

        // Child streams: SIGNATURE_SIZE = 13 bytes header, then payload.
        let mut x_stream = vec![0u8; 13];
        x_stream.extend_from_slice(&0.25f64.to_le_bytes());
        let mut objective_stream = vec![0u8; 13];
        objective_stream.extend_from_slice(&63.0f64.to_le_bytes());
        let mut detector_stream = vec![0u8; 13];
        detector_stream.extend_from_slice(&4i32.to_le_bytes());
        detector_stream.extend_from_slice(b"CCD1");
        let mut timestamp_stream = vec![0u8; 22];
        timestamp_stream.extend_from_slice(&777i32.to_le_bytes());

        let samples = vec![
            volocity_test_sample(1, 0, 1, 1, None, None),
            volocity_test_sample(2, 1, 1, 2, Some(volocity_test_inline_stream(4, 5, 2)), None),
            volocity_test_sample(3, 2, 0, 3, Some(x_stream), None),
            volocity_test_sample(4, 2, 0, 4, Some(objective_stream), None),
            volocity_test_sample(5, 2, 0, 5, Some(detector_stream), None),
            volocity_test_sample(6, 2, 0, 6, Some(timestamp_stream), None),
        ];

        let candidates = volocity_stack_candidates(&samples, &strings, &[], None);
        assert_eq!(candidates.len(), 1);
        let metadata = &candidates[0].metadata;
        assert_eq!(metadata.physical_x, Some(0.25));
        assert_eq!(metadata.magnification, Some(63.0));
        assert_eq!(metadata.detector.as_deref(), Some("CCD1"));
        assert_eq!(metadata.timestamp_atsf_id, Some(777));

        let probe = VolocityMetakitProbe {
            little_endian: true,
            footer_offset: 0,
            toc_offset: 0,
            structure_len: 0,
            tables: Vec::new(),
            stack_candidates: candidates,
        };
        let summary = volocity_native_semantic_summary(&probe).unwrap();
        assert!(summary.contains("physicalX=0.25"));
        assert!(summary.contains("magnification=63"));
        assert!(summary.contains("detector=\"CCD1\""));
        assert!(summary.contains("timestamp_atsf=777.atsf"));
    }

    #[test]
    fn volocity_native_metakit_error_reports_table_shape() {
        let path = temp_dir("native.mvd2");
        std::fs::write(&path, include_bytes!("../metakit/tests/data/test.mk")).unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("native Metakit decoding is unsupported")
                    && message.contains("footer=22569")
                    && message.contains("toc=1496")
                    && message.contains("structure=488B")
                    && message.contains("table_count=4")
                    && message.contains("variablesView(1)[varVersion:I")
                    && message.contains("samplesViewR(29)[sampleID:I")
                    && message.contains("stringsViewR(23)[stringID:I|stringString:S|stringRefCount:I]")
                    && message.contains("filesViewR(0)[fileID:I|fileName:S|fileSpec:B|fileRefCount:I]")
                    && message.contains("single-row scalars: variablesView.varVersion=2")
                    && message.contains("variablesView.varNextStringID=24")
                    && message.contains("Java metadata roles: variables=variablesView(1), samples=samplesViewR(29), strings=stringsViewR(23), files=filesViewR(0)")
                    && message.contains("variables version=2, next_sample_id=30, next_string_id=24, next_file_id=2")
                    && message.contains("sample hierarchy id=sampleID, parent=sampleParent")
                    && message.contains("file_link=sampleFileLink, name_link=sampleNameLink")
                    && message.contains("first sample row sampleID=1, sampleParent=0")
                    && message.contains("sampleChildType=1 (Java stack-candidate branch)")
                    && message.contains("sampleData=21 bytes hex=493100020058020000f501020064000000f7010000")
                    && message.contains("string links stringsViewR[stringID, stringString, stringRefCount]")
                    && message.contains("first string links row stringID=1, stringString=\"clipping-test.mvd2\\0\"")
                    && message.contains("file links filesViewR[fileID, fileName, fileSpec, fileRefCount]")
                    && message.contains("Java stack candidates 1: sampleID=5, name=\"Tx red 2\"")
                    && message.contains("external_data=1")
                    && message.contains("native.mvd2")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn volocity_native_error_reports_bounded_companion_provenance() {
        let root = temp_dir("native-companions");
        let stack_dir = root.join("Data").join("Stack");
        std::fs::create_dir_all(&stack_dir).unwrap();
        std::fs::write(stack_dir.join("1.aisf"), b"aisf").unwrap();
        let path = root.join("Library.mvd2");
        std::fs::write(&path, include_bytes!("../metakit/tests/data/test.mk")).unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("companion provenance: Data directory present")
                    && message.contains("stack sampleID=5 external_data=1 1.aisf=Data/Stack/1.aisf")
                    && message.contains("stack sampleID=5 external_data=1 1.aiix=missing")
                    && message.contains("stack sampleID=5 external_data=1 1.dat=missing")
                    && message.contains("stack sampleID=5 external_data=1 1.atsf=missing")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn volocity_truncated_metakit_signature_has_explicit_error() {
        let path = temp_dir("truncated-native.mvd2");
        std::fs::write(&path, b"JL").unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("Metakit stream signature was present")
                    && message.contains("Metakit header is truncated")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn volocity_companion_detection_requires_owning_mvd2() {
        let root = temp_dir("companion");
        let library = root.join("Library");
        let stack_dir = library.join("Data").join("Stack");
        std::fs::create_dir_all(&stack_dir).unwrap();
        let companion = stack_dir.join("1.aisf");
        std::fs::write(&companion, b"JL").unwrap();

        let reader = VolocityReader::new();
        assert!(!reader.is_this_type_by_name(&companion));

        let mvd2 = library.join("Library.mvd2");
        std::fs::write(&mvd2, b"JL").unwrap();
        assert!(reader.is_this_type_by_name(&mvd2));
        assert!(reader.is_this_type_by_name(&companion));

        let err = VolocityReader::new().set_id(&companion).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("native Metakit decoding is unsupported")
                    && message.contains("Library.mvd2")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn volocity_set_id_uses_java_initfile_companion_search() {
        let root = temp_dir("companion-init");
        let library = root.join("Library");
        let data_dir = library.join("Data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let companion = data_dir.join("1.dat");
        std::fs::write(&companion, b"pixels").unwrap();
        let mvd2 = library.join("Library.mvd2");
        std::fs::write(&mvd2, include_bytes!("../metakit/tests/data/test.mk")).unwrap();

        let reader = VolocityReader::new();
        // Java isThisType walks three parents and requires
        // "<parent>/<parent>.mvd2"; for this direct Data child that is false.
        assert!(!reader.is_this_type_by_name(&companion));

        // Java initFile is looser: it walks two parents and recursively picks
        // the .mvd2 under that directory. set_id must therefore route to the
        // Metakit library instead of reporting the companion path itself.
        let err = VolocityReader::new().set_id(&companion).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("native Metakit decoding is unsupported")
                    && message.contains("Library.mvd2")
                    && !message.contains("1.dat")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn volocity_native_region_uses_rgb_channel_count_not_total_size_c() {
        let stack = VolocityStack {
            pixels_files: vec![VolocityPixels::Embedded(vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
            ])],
            timestamp_file: None,
            plane_padding: 0,
            block_size: 0,
            clipping_data: false,
            channel_names: vec!["R".to_string(), "G".to_string()],
            name: "RGB split".to_string(),
            description: None,
            size_x: 2,
            size_y: 2,
            size_z: 1,
            // Java keeps imageCount at sizeZ * original_channel_count * sizeT
            // but multiplies sizeC by 3 for RGB .aisf stacks; the effective
            // crop samples-per-pixel is therefore sizeC / effectiveSizeC = 3.
            size_c: 6,
            size_t: 1,
            image_count: 2,
            pixel_type: PixelType::Uint8,
            rgb: true,
            little_endian: true,
        };
        let mut reader = VolocityReader {
            path: None,
            meta: None,
            bytes: Vec::new(),
            layout: None,
            stacks: vec![stack.clone()],
            series_meta: vec![volocity_stack_metadata(&stack)],
            current_series: 0,
        };

        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            vec![4, 5, 6, 10, 11, 12]
        );
    }

    fn blind_mvd2(width: u32, height: u32, z: u32, c: u32, t: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(VOLOCITY_BLIND_MAGIC);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&z.to_le_bytes());
        bytes.extend_from_slice(&c.to_le_bytes());
        bytes.extend_from_slice(&t.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(VOLOCITY_BLIND_HEADER_LEN as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn volocity_reads_strict_blind_raw_subset() {
        let path = temp_dir("blind.mvd2");
        let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];
        std::fs::write(&path, blind_mvd2(2, 2, 2, 1, 1, &payload)).unwrap();

        let mut reader = VolocityReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("volocity_version_subset"),
            Some(MetadataValue::String(value))
                if value == "BFVOLOCITYMVD2-blind-raw-v1"
        ));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("Volocity blind data offset"),
            Some(MetadataValue::Int(value)) if *value == VOLOCITY_BLIND_HEADER_LEN as i64
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn volocity_blind_subset_rejects_truncated_payload() {
        let path = temp_dir("truncated.mvd2");
        std::fs::write(&path, blind_mvd2(2, 2, 1, 1, 1, &[1, 2, 3])).unwrap();

        let err = VolocityReader::new().set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message) if message.contains("payload length")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn volocity_suffix_contract_matches_java_reader() {
        for suffix in VOLOCITY_SUFFIXES {
            assert!(!suffix.is_empty());
        }
        let reader = VolocityReader::new();
        assert!(reader.is_this_type_by_name(Path::new("sample.mvd2")));
        assert!(!reader.is_this_type_by_name(Path::new("orphan.aisf")));
    }

    #[test]
    fn nikon_nis_claims_only_nif_name() {
        let reader = NikonNisReader::new();
        assert!(reader.is_this_type_by_name(Path::new("sample.nif")));
        assert!(reader.is_this_type_by_name(Path::new("sample.NIF")));
        assert!(!reader.is_this_type_by_name(Path::new("sample.nd2")));
        assert!(!reader.is_this_type_by_name(Path::new("sample.nef")));
        assert!(!reader.is_this_type_by_bytes(b"II*\0"));
    }
}

impl NikonNisReader {
    pub fn new() -> Self {
        NikonNisReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}
impl Default for NikonNisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonNisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Nikon NIS `.nif` is TIFF-backed. `.nd2` is handled by the native ND2
        // reader, and Java's `NikonReader` is NEF/TIFF camera RAW, not NIS.
        matches!(ext.as_deref(), Some("nif"))
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool {
        false
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, l: usize) -> Result<()> {
        self.inner.set_resolution(l)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}
