//! LaVision UltraMicroscope stage positions, recovered from the vendor
//! `<CustomAttributes>` elements that the OME model has no slot for.
//!
//! # This is a deliberate deviation from Java Bio-Formats
//!
//! Java Bio-Formats returns **no** stage position for these files, and that is
//! not a defect in it. LaVision's UltraII writes a self-declaring OME-TIFF, so
//! Bio-Formats dispatches to `OMETiffReader`, which trusts the OME-XML. The XML
//! carries no `<Plane>` elements and no `<StageLabel>`, so
//! `getPlanePositionX(0, 0)` throws `IndexOutOfBoundsException` and
//! `getStageLabelX(0)` returns null. Verified directly against
//! `bioformats_package.jar` on a real UltraII pair.
//!
//! The positions do exist, as **structured XML**, in two `<CustomAttributes>`
//! elements that the OME model has no place for:
//!
//! * `OME/Image/CustomAttributes` — 23 acquisition elements, including
//!   `<Offset>` (this file's stage origin) and `<TileConfiguration>`;
//! * `OME/CustomAttributes` — a `<PropArray>` of ~844 instrument properties.
//!
//! Both sit in LaVision's own namespace (`Schemas/CA/2008-02`), which is why
//! `OMETiffReader` — which reads the OME schema — walks straight past them.
//! They are *not* free text inside `<Description>`; an earlier version of this
//! comment said so and was wrong. What this module reads:
//!
//! ```xml
//! <Offset Offset_0="-1861.449219" Offset_1="-5300.662598" Offset_2="0.0" Offset_3="0.0"/>
//! <TileConfiguration TileConfiguration="3
//! ..._UltraII[00 x 00]_C00_UltraII Filter0000.ome.tif;;(-4815.699219, 4234.837402, 0.000000)
//! ..."/>
//! ```
//!
//! `<Offset>` is this file's own stage origin; `<TileConfiguration>` is the
//! whole acquisition's tile layout, repeated in every file of the set. On a
//! real pair, `[05 x 05]` and `[05 x 06]` differ in `Offset_0` by 590.85 um
//! against a 656.5 um tile (404 px x 1.625 um/px) — a 10.0 % overlap, which
//! independently agrees with the overlap the file states directly (see "What
//! this module is *not* for", below).
//!
//! So this module makes `bioformats-rs` return stage positions where Java
//! returns none. **Callers comparing the two libraries will see a difference
//! here, and it is intentional.** Removing it to restore strict parity would
//! discard information that is present in the file. See `README.md`,
//! "Intentional deviations from Java Bio-Formats".
//!
//! Nothing here fires unless the XML actually contains the LaVision markers, so
//! non-LaVision OME-TIFFs are byte-for-byte unaffected.
//!
//! # What this module is *not* for
//!
//! It does **not** carry the tile overlap. That is a separate property,
//! `xyz-Table_X_Overlap` / `_Y_Overlap`, inside the `<PropArray>` of the root
//! `OME/CustomAttributes`, and it is stated outright by the file
//! (65.650002 um, and `xyz-Table_XY_Overlap` = 10 %). Consumers that need the
//! overlap should read that property rather than deriving it from the offsets
//! here — and note that ClearMap does exactly that, so deriving it instead
//! would diverge from upstream.

use std::collections::HashMap;

use crate::common::metadata::MetadataValue;

/// One tile's entry in the acquisition-wide `<TileConfiguration>`.
#[derive(Debug, Clone, PartialEq)]
pub struct LaVisionTile {
    /// File name exactly as written in the configuration line.
    pub file_name: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Stage geometry recovered from a LaVision `<CustomAttributes>` block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaVisionStageMetadata {
    /// This file's stage origin, from `<Offset Offset_0/_1/_2>`. Micrometres.
    pub offset_x: Option<f64>,
    pub offset_y: Option<f64>,
    pub offset_z: Option<f64>,
    /// The acquisition's full tile layout, when `<TileConfiguration>` is present.
    pub tiles: Vec<LaVisionTile>,
}

impl LaVisionStageMetadata {
    pub fn is_empty(&self) -> bool {
        self.offset_x.is_none()
            && self.offset_y.is_none()
            && self.offset_z.is_none()
            && self.tiles.is_empty()
    }
}

/// True when `xml` looks like a LaVision UltraMicroscope description.
///
/// Deliberately narrow: both markers must be present. `xyz-Table` is LaVision's
/// stage-control prefix and appears dozens of times in a genuine file; `<Offset`
/// alone is far too generic to gate on.
fn looks_like_lavision(xml: &str) -> bool {
    xml.contains("xyz-Table") && xml.contains("<Offset")
}

/// Parse the LaVision stage block out of an OME-XML document.
///
/// Returns `None` for any XML that is not recognisably LaVision, and for
/// LaVision XML that yields no usable geometry.
pub fn parse_lavision_stage(xml: &str) -> Option<LaVisionStageMetadata> {
    if !looks_like_lavision(xml) {
        return None;
    }

    let mut meta = LaVisionStageMetadata::default();

    if let Some(tag) = find_element(xml, "<Offset ") {
        meta.offset_x = attribute(tag, "Offset_0").and_then(|v| v.parse().ok());
        meta.offset_y = attribute(tag, "Offset_1").and_then(|v| v.parse().ok());
        meta.offset_z = attribute(tag, "Offset_2").and_then(|v| v.parse().ok());
    }

    if let Some(tag) = find_element(xml, "<TileConfiguration") {
        if let Some(body) = attribute(tag, "TileConfiguration") {
            meta.tiles = parse_tile_configuration(body);
        }
    }

    (!meta.is_empty()).then_some(meta)
}

/// Return the full text of the first element beginning with `open`, including
/// its closing `>`.
fn find_element<'a>(xml: &'a str, open: &str) -> Option<&'a str> {
    let start = xml.find(open)?;
    let rest = &xml[start..];
    let end = rest.find('>')?;
    Some(&rest[..=end])
}

/// Read `name="value"` out of a single element's text.
fn attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(hit) = element[from..].find(name) {
        let at = from + hit;
        let before_ok = at == 0
            || element[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let after = &element[at + name.len()..];
        // Guard against `Offset_0` matching the prefix of `Offset_01`, and
        // against `TileConfiguration` inside `<TileConfiguration ...>`.
        if before_ok && after.starts_with("=\"") {
            let value = &after[2..];
            return value.find('"').map(|end| &value[..end]);
        }
        from = at + name.len();
    }
    None
}

/// Parse the newline-separated body of `TileConfiguration`.
///
/// Each tile line is `"<file name>;;(x, y, z)"`. The first line is a bare
/// dimension count (`3`) and is skipped, as is anything that does not match.
fn parse_tile_configuration(body: &str) -> Vec<LaVisionTile> {
    let mut tiles = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some((name, coords)) = line.split_once(";;") else {
            continue;
        };
        let coords = coords.trim();
        let Some(inner) = coords
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        let parts: Vec<f64> = inner
            .split(',')
            .filter_map(|p| p.trim().parse::<f64>().ok())
            .collect();
        if parts.len() == 3 {
            tiles.push(LaVisionTile {
                file_name: name.trim().to_string(),
                x: parts[0],
                y: parts[1],
                z: parts[2],
            });
        }
    }
    tiles
}

/// Publish the recovered geometry into a series metadata table.
///
/// Keys are prefixed `LaVision ` so they are visibly vendor-derived and cannot
/// collide with anything Java Bio-Formats emits.
pub fn populate_series_metadata(
    meta: &LaVisionStageMetadata,
    table: &mut HashMap<String, MetadataValue>,
) {
    if let Some(x) = meta.offset_x {
        table.insert("LaVision StagePositionX".into(), MetadataValue::Float(x));
    }
    if let Some(y) = meta.offset_y {
        table.insert("LaVision StagePositionY".into(), MetadataValue::Float(y));
    }
    if let Some(z) = meta.offset_z {
        table.insert("LaVision StagePositionZ".into(), MetadataValue::Float(z));
    }
    if !meta.tiles.is_empty() {
        table.insert(
            "LaVision TileCount".into(),
            MetadataValue::Int(meta.tiles.len() as i64),
        );
        for (index, tile) in meta.tiles.iter().enumerate() {
            table.insert(
                format!("LaVision Tile{index} FileName"),
                MetadataValue::String(tile.file_name.clone()),
            );
            table.insert(
                format!("LaVision Tile{index} PositionX"),
                MetadataValue::Float(tile.x),
            );
            table.insert(
                format!("LaVision Tile{index} PositionY"),
                MetadataValue::Float(tile.y),
            );
            table.insert(
                format!("LaVision Tile{index} PositionZ"),
                MetadataValue::Float(tile.z),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real UltraII file, preserving its actual shape: the stage
    /// geometry lives in `OME/Image/CustomAttributes`, and the `xyz-Table`
    /// marker in the root `OME/CustomAttributes/PropArray`.
    const SAMPLE: &str = r#"<OME xmlns:ca="http://www.openmicroscopy.org/Schemas/CA/2008-02">
  <Image>
    <ca:CustomAttributes>
      <Offset Offset_0="-1861.449219" Offset_1="-5300.662598" Offset_2="0.0" Offset_3="0.0"/>
      <TileConfiguration TileConfiguration="3
14-16-41_tricocktail_UltraII[00 x 00]_C00_UltraII Filter0000.ome.tif;;(-4815.699219, 4234.837402,0.000000)
14-16-41_tricocktail_UltraII[05 x 06]_C00_UltraII Filter0000.ome.tif;;(-1270.599365, -5300.662598,0.000000)
"/>
    </ca:CustomAttributes>
  </Image>
  <ca:CustomAttributes>
    <PropArray>
      <xyz-Table_X_Overlap nTy="3" nId="524293" fname="xyz-Table XOvl" Value="65.650002"/>
    </PropArray>
  </ca:CustomAttributes>
</OME>"#;

    #[test]
    fn recovers_the_stage_offset_java_bioformats_does_not_expose() {
        let meta = parse_lavision_stage(SAMPLE).expect("LaVision block should be recognised");
        assert_eq!(meta.offset_x, Some(-1861.449219));
        assert_eq!(meta.offset_y, Some(-5300.662598));
        assert_eq!(meta.offset_z, Some(0.0));
    }

    #[test]
    fn recovers_the_acquisition_tile_layout() {
        let meta = parse_lavision_stage(SAMPLE).unwrap();
        assert_eq!(meta.tiles.len(), 2);
        assert_eq!(meta.tiles[0].x, -4815.699219);
        assert_eq!(meta.tiles[0].y, 4234.837402);
        assert!(meta.tiles[1].file_name.contains("[05 x 06]"));
        assert_eq!(meta.tiles[1].x, -1270.599365);
    }

    /// The measurement that motivates this module: the X step between adjacent
    /// tiles, against a 404 px x 1.625 um tile, is a 10 % overlap.
    #[test]
    fn adjacent_tile_offsets_yield_the_expected_overlap() {
        let meta = parse_lavision_stage(SAMPLE).unwrap();
        let step = (meta.tiles[1].x - meta.offset_x.unwrap()).abs();
        let tile_um = 404.0 * 1.625;
        let overlap = (tile_um - step) / tile_um;
        assert!(
            (overlap - 0.10).abs() < 0.005,
            "expected ~10% overlap, got {overlap}"
        );
    }

    #[test]
    fn plain_ome_xml_is_left_alone() {
        let plain = r#"<OME><Image><Pixels SizeX="404" SizeY="1304"/></Image></OME>"#;
        assert!(parse_lavision_stage(plain).is_none());
    }

    /// `<Offset` alone must not trigger: it is far too generic a tag name to
    /// claim a file is LaVision.
    #[test]
    fn offset_without_the_stage_marker_is_not_lavision() {
        let other = r#"<OME><CustomAttributes><Offset Offset_0="1.0"/></CustomAttributes></OME>"#;
        assert!(parse_lavision_stage(other).is_none());
    }

    #[test]
    fn attribute_lookup_does_not_match_a_longer_name() {
        let element = r#"<Offset Offset_01="9.0" Offset_0="1.5"/>"#;
        assert_eq!(attribute(element, "Offset_0"), Some("1.5"));
    }

    #[test]
    fn series_metadata_keys_are_vendor_prefixed() {
        let meta = parse_lavision_stage(SAMPLE).unwrap();
        let mut table = HashMap::new();
        populate_series_metadata(&meta, &mut table);
        assert_eq!(
            table.get("LaVision StagePositionX").map(|v| v.to_string()),
            Some("-1861.449219".to_string())
        );
        assert_eq!(
            table.get("LaVision TileCount").map(|v| v.to_string()),
            Some("2".to_string())
        );
        assert!(table.keys().all(|k| k.starts_with("LaVision ")));
    }
}
