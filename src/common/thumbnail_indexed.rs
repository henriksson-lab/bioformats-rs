//! OpenJDK's indexed destination conversion used by AWTImageTools.scale.
//!
//! Bio-Formats temporarily replaces the source palette with a gray color model,
//! then draws the area-averaged RGB image into the original indexed model. The
//! destination conversion is OpenJDK ByteIndexed.h / libawt initCubemap,
//! initInverseGrayLut and initDitherTables, rather than an RGB nearest search.

use crate::common::metadata::LookupTable;

pub(super) struct IndexedThumbnail {
    inverse: Vec<u8>,
    gray: Option<[u8; 256]>,
    represents_primaries: bool,
}

impl IndexedThumbnail {
    pub(super) fn new(table: &LookupTable) -> Self {
        let colors: Vec<[u8; 3]> = (0..table.red.len().min(256))
            .map(|i| {
                [
                    table.red[i] as u8,
                    table.green.get(i).copied().unwrap_or(0) as u8,
                    table.blue.get(i).copied().unwrap_or(0) as u8,
                ]
            })
            .collect();
        let mut inverse = vec![0; 32 * 32 * 32];
        let mut seen = vec![false; inverse.len()];
        let mut frontier = Vec::new();
        // initCubemap alternates the palette's ends; duplicate cells retain the
        // first seed. Successive distance shells visit the prior shell backwards.
        for i in 0..colors.len().div_ceil(2) {
            for index in [i, colors.len() - 1 - i] {
                let [r, g, b] = colors[index];
                let cell = ((r as usize >> 3) << 10) | ((g as usize >> 3) << 5) | (b as usize >> 3);
                if !seen[cell] {
                    seen[cell] = true;
                    inverse[cell] = index as u8;
                    frontier.push(cell);
                }
            }
        }
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for &cell in frontier.iter().rev() {
                let r = cell >> 10;
                let g = (cell >> 5) & 31;
                let b = cell & 31;
                // Neighbor order determines equal-distance palette ties.
                for (inside, delta) in [
                    (r < 31, 1024),
                    (r > 0, -1024),
                    (g < 31, 32),
                    (g > 0, -32),
                    (b < 31, 1),
                    (b > 0, -1),
                ] {
                    if inside {
                        let adjacent = (cell as isize + delta) as usize;
                        if !seen[adjacent] {
                            seen[adjacent] = true;
                            inverse[adjacent] = inverse[cell];
                            next.push(adjacent);
                        }
                    }
                }
            }
            frontier = next;
        }
        let gray = if !colors.is_empty() && colors.iter().all(|&[r, g, b]| r == g && g == b) {
            let mut entries = [-1i16; 256];
            // Native inverse-gray initialization replaces earlier duplicates.
            for (i, &[value, _, _]) in colors.iter().enumerate() {
                entries[value as usize] = i as i16;
            }
            let mut prior_value: Option<usize> = None;
            let mut prior_index = -1;
            let mut gap = false;
            for value in 0..256 {
                let index = entries[value];
                if index < 0 {
                    entries[value] = prior_index;
                    gap = true;
                } else {
                    if gap {
                        // OpenJDK rounds this midpoint down, including odd gaps.
                        let midpoint = prior_value.map(|v| (v + value) / 2).unwrap_or(0);
                        entries[midpoint..value].fill(index);
                    }
                    prior_value = Some(value);
                    prior_index = index;
                    gap = false;
                }
            }
            Some(entries.map(|v| v.max(0) as u8))
        } else {
            None
        };
        let represents_primaries = (0..8).all(|corner| {
            colors.contains(&[
                if corner & 4 != 0 { 255 } else { 0 },
                if corner & 2 != 0 { 255 } else { 0 },
                if corner & 1 != 0 { 255 } else { 0 },
            ])
        });
        Self {
            inverse,
            gray,
            represents_primaries,
        }
    }

    pub(super) fn index(&self, gray: u8, x: usize, y: usize) -> u8 {
        if let Some(inverse) = &self.gray {
            return inverse[gray as usize];
        }
        let pos = ((y & 7) << 3) | (x & 7);
        let skip = self.represents_primaries && (gray == 0 || gray == 255);
        let component = |errors: &[i8; 64]| -> usize {
            ((gray as i16 + if skip { 0 } else { errors[pos] as i16 }).clamp(0, 255) as usize) >> 3
        };
        self.inverse[(component(&RED) << 10) | (component(&GREEN) << 5) | component(&BLUE)]
    }
}

// initDitherTables' 8x8 signed ordered errors for a 256-entry palette.
// Green mirrors red horizontally; blue mirrors it vertically.
const RED: [i8; 64] = [
    -20, 0, -15, 5, -19, 1, -14, 6, 10, -10, 15, -5, 11, -9, 16, -4, -13, 7, -18, 2, -12, 8, -17,
    3, 17, -3, 12, -8, 18, -2, 13, -7, -19, 1, -14, 6, -20, 0, -15, 5, 11, -9, 16, -4, 10, -10, 15,
    -5, -11, 9, -16, 4, -12, 8, -17, 3, 19, -1, 14, -6, 18, -2, 13, -7,
];
const GREEN: [i8; 64] = [
    6, -14, 1, -19, 5, -15, 0, -20, -4, 16, -9, 11, -5, 15, -10, 10, 3, -17, 8, -12, 2, -18, 7,
    -13, -7, 13, -2, 18, -8, 12, -3, 17, 5, -15, 0, -20, 6, -14, 1, -19, -5, 15, -10, 10, -4, 16,
    -9, 11, 3, -17, 8, -12, 4, -16, 9, -11, -7, 13, -2, 18, -6, 14, -1, 19,
];
const BLUE: [i8; 64] = [
    19, -1, 14, -6, 18, -2, 13, -7, -11, 9, -16, 4, -12, 8, -17, 3, 11, -9, 16, -4, 10, -10, 15,
    -5, -19, 1, -14, 6, -20, 0, -15, 5, 17, -3, 12, -8, 18, -2, 13, -7, -13, 7, -18, 2, -12, 8,
    -17, 3, 10, -10, 15, -5, 11, -9, 16, -4, -20, 0, -15, 5, -19, 1, -14, 6,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_destination_matches_java_all_gray_values_and_dither_positions() {
        // Goldens: tests/fixtures/thumbnail/PaletteGrid.java, OpenJDK 19.0.2.
        let golden = [
            0x64b765ba33ddf325u64,
            0x2d5a6076a95d0325,
            0x093957b891131325,
            0xdd4e54e0dcb19755,
            0xd9669a7c0fe22d42,
            0xcfa7aad894d698b5,
            0xe23647a0aa7a4cd5,
            0xed2f4ec71777ea95,
            0xed2f4ec71777ea95,
            0x2413e2057b3e1ce5,
        ];
        for (mode, &expected) in golden.iter().enumerate() {
            let n = match mode {
                0..=2 => 256,
                3 => 8,
                4 => 7,
                5 => 2,
                9 => 5,
                _ => 262 - mode,
            };
            let mut table = LookupTable {
                red: Vec::new(),
                green: Vec::new(),
                blue: Vec::new(),
            };
            for i in 0..n {
                let [r, g, b] = if mode == 9 {
                    [[0; 3], [9; 3], [9; 3], [11; 3], [255; 3]][i]
                } else if mode < 3 {
                    let v = match mode {
                        0 => i,
                        1 => 255 - i,
                        _ => (i * 73) & 255,
                    };
                    [v; 3]
                } else if mode >= 6 && i < 248 {
                    [i; 3]
                } else {
                    let v = if mode >= 6 { i - 248 } else { i };
                    [
                        if v & 4 != 0 { 255 } else { 0 },
                        if v & 2 != 0 { 255 } else { 0 },
                        if v & 1 != 0 { 255 } else { 0 },
                    ]
                };
                table.red.push(r as u16);
                table.green.push(g as u16);
                table.blue.push(b as u16);
            }
            let indexed = IndexedThumbnail::new(&table);
            let mut digest = 0xcbf29ce484222325u64;
            for y in 0..8 {
                for x in 0..2048 {
                    digest = (digest ^ indexed.index((x / 8) as u8, x, y) as u64)
                        .wrapping_mul(0x100000001b3);
                }
            }
            assert_eq!(digest, expected, "palette {mode}");
        }
    }
}
