//! Reader wrappers that transform pixel data or metadata on the fly.
//!
//! Equivalent to Java Bio-Formats' `ReaderWrapper` hierarchy:
//! `ChannelSeparator`, `ChannelMerger`, `ChannelFiller`, `DimensionSwapper`,
//! `MinMaxCalculator`.

use std::path::Path;
use crate::common::error::Result;
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::OmeMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── ChannelSeparator ────────────────────────────────────────────────────────

/// Splits interleaved RGB planes into separate per-channel planes.
///
/// If the underlying reader returns interleaved RGB data (3 channels per plane),
/// the ChannelSeparator presents each channel as its own plane.
/// For a reader with `image_count = N` interleaved RGB planes, the separator
/// exposes `image_count = N * C` planes, where C is the channel count.
///
/// Non-RGB readers pass through unchanged.
pub struct ChannelSeparator {
    inner: Box<dyn FormatReader>,
    adjusted_meta: Option<ImageMetadata>,
}

impl ChannelSeparator {
    pub fn new(inner: Box<dyn FormatReader>) -> Self {
        ChannelSeparator { inner, adjusted_meta: None }
    }

    fn rebuild_meta(&mut self) {
        let meta = self.inner.metadata();
        if meta.is_rgb && meta.is_interleaved && meta.size_c > 1 {
            let mut adjusted = meta.clone();
            adjusted.image_count = meta.image_count * meta.size_c;
            adjusted.is_interleaved = false;
            adjusted.is_rgb = false;
            self.adjusted_meta = Some(adjusted);
        } else {
            self.adjusted_meta = None;
        }
    }

    /// Extract one channel from interleaved pixel data.
    fn extract_channel(data: &[u8], channel: usize, n_channels: usize, bps: usize) -> Vec<u8> {
        let pixel_bytes = n_channels * bps;
        let n_pixels = data.len() / pixel_bytes;
        let mut out = Vec::with_capacity(n_pixels * bps);
        for i in 0..n_pixels {
            let offset = i * pixel_bytes + channel * bps;
            out.extend_from_slice(&data[offset..offset + bps]);
        }
        out
    }
}

impl FormatReader for ChannelSeparator {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize { self.inner.series_count() }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)?;
        self.rebuild_meta();
        Ok(())
    }

    fn series(&self) -> usize { self.inner.series() }

    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta.as_ref().unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_split, nc, bps) = {
            let meta = self.inner.metadata();
            (meta.is_rgb && meta.is_interleaved && meta.size_c > 1,
             meta.size_c, meta.pixel_type.bytes_per_sample())
        };
        if is_split {
            let real_plane = plane_index / nc;
            let channel = (plane_index % nc) as usize;
            let data = self.inner.open_bytes(real_plane)?;
            Ok(Self::extract_channel(&data, channel, nc as usize, bps))
        } else {
            self.inner.open_bytes(plane_index)
        }
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let (is_split, nc, bps) = {
            let meta = self.inner.metadata();
            (meta.is_rgb && meta.is_interleaved && meta.size_c > 1,
             meta.size_c, meta.pixel_type.bytes_per_sample())
        };
        if is_split {
            let real_plane = plane_index / nc;
            let channel = (plane_index % nc) as usize;
            let data = self.inner.open_bytes_region(real_plane, x, y, w, h)?;
            Ok(Self::extract_channel(&data, channel, nc as usize, bps))
        } else {
            self.inner.open_bytes_region(plane_index, x, y, w, h)
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_split, nc, bps) = {
            let meta = self.inner.metadata();
            (meta.is_rgb && meta.is_interleaved && meta.size_c > 1,
             meta.size_c, meta.pixel_type.bytes_per_sample())
        };
        if is_split {
            let real_plane = plane_index / nc;
            let channel = (plane_index % nc) as usize;
            let data = self.inner.open_thumb_bytes(real_plane)?;
            Ok(Self::extract_channel(&data, channel, nc as usize, bps))
        } else {
            self.inner.open_thumb_bytes(plane_index)
        }
    }

    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
    fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
}

// ── ChannelMerger ───────────────────────────────────────────────────────────

/// Merges separate per-channel planes into interleaved RGB planes.
///
/// If the underlying reader returns separate grayscale planes for each channel,
/// the ChannelMerger reads N consecutive planes and interleaves them into one
/// RGB plane. Exposes `image_count = original_count / C`.
///
/// Only activates when the reader has multiple channels that are NOT interleaved.
pub struct ChannelMerger {
    inner: Box<dyn FormatReader>,
    adjusted_meta: Option<ImageMetadata>,
}

impl ChannelMerger {
    pub fn new(inner: Box<dyn FormatReader>) -> Self {
        ChannelMerger { inner, adjusted_meta: None }
    }

    fn rebuild_meta(&mut self) {
        let meta = self.inner.metadata();
        if !meta.is_rgb && !meta.is_interleaved && meta.size_c > 1 && meta.image_count > 1 {
            let mut adjusted = meta.clone();
            adjusted.image_count = meta.image_count / meta.size_c;
            adjusted.is_rgb = true;
            adjusted.is_interleaved = true;
            self.adjusted_meta = Some(adjusted);
        } else {
            self.adjusted_meta = None;
        }
    }

    /// Interleave N channel buffers into one RGBRGB... buffer.
    fn interleave(channels: &[Vec<u8>], bps: usize) -> Vec<u8> {
        if channels.is_empty() { return Vec::new(); }
        let nc = channels.len();
        let n_pixels = channels[0].len() / bps;
        let mut out = Vec::with_capacity(n_pixels * nc * bps);
        for i in 0..n_pixels {
            for ch in channels {
                let offset = i * bps;
                out.extend_from_slice(&ch[offset..offset + bps]);
            }
        }
        out
    }
}

impl FormatReader for ChannelMerger {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize { self.inner.series_count() }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)?;
        self.rebuild_meta();
        Ok(())
    }

    fn series(&self) -> usize { self.inner.series() }

    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta.as_ref().unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_merge, nc, bps) = {
            let meta = self.inner.metadata();
            (self.adjusted_meta.is_some(), meta.size_c, meta.pixel_type.bytes_per_sample())
        };
        if is_merge {
            let mut channels = Vec::with_capacity(nc as usize);
            for c in 0..nc {
                channels.push(self.inner.open_bytes(plane_index * nc + c)?);
            }
            Ok(Self::interleave(&channels, bps))
        } else {
            self.inner.open_bytes(plane_index)
        }
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let (is_merge, nc, bps) = {
            let meta = self.inner.metadata();
            (self.adjusted_meta.is_some(), meta.size_c, meta.pixel_type.bytes_per_sample())
        };
        if is_merge {
            let mut channels = Vec::with_capacity(nc as usize);
            for c in 0..nc {
                channels.push(self.inner.open_bytes_region(plane_index * nc + c, x, y, w, h)?);
            }
            Ok(Self::interleave(&channels, bps))
        } else {
            self.inner.open_bytes_region(plane_index, x, y, w, h)
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_merge, nc, bps) = {
            let meta = self.inner.metadata();
            (self.adjusted_meta.is_some(), meta.size_c, meta.pixel_type.bytes_per_sample())
        };
        if is_merge {
            let mut channels = Vec::with_capacity(nc as usize);
            for c in 0..nc {
                channels.push(self.inner.open_thumb_bytes(plane_index * nc + c)?);
            }
            Ok(Self::interleave(&channels, bps))
        } else {
            self.inner.open_thumb_bytes(plane_index)
        }
    }

    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
    fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
}

// ── DimensionSwapper ────────────────────────────────────────────────────────

/// Swaps dimension order of the metadata, remapping plane indices accordingly.
///
/// The underlying pixel data is unchanged — only the interpretation of which
/// plane corresponds to which (Z, C, T) coordinate changes.
pub struct DimensionSwapper {
    inner: Box<dyn FormatReader>,
    target_order: DimensionOrder,
    adjusted_meta: Option<ImageMetadata>,
}

impl DimensionSwapper {
    pub fn new(inner: Box<dyn FormatReader>, target_order: DimensionOrder) -> Self {
        DimensionSwapper { inner, target_order, adjusted_meta: None }
    }

    fn rebuild_meta(&mut self) {
        let meta = self.inner.metadata();
        let mut adjusted = meta.clone();
        adjusted.dimension_order = self.target_order;
        self.adjusted_meta = Some(adjusted);
    }

    /// Convert a linear plane index from the target dimension order to the
    /// source dimension order.
    fn remap_plane(&self, plane_index: u32) -> u32 {
        let meta = self.inner.metadata();
        let (sz, sc, st) = (meta.size_z, meta.size_c, meta.size_t);

        // Decompose plane_index according to target order
        let (z, c, t) = decompose_plane(plane_index, sz, sc, st, self.target_order);
        // Recompose according to source order
        compose_plane(z, c, t, sz, sc, st, meta.dimension_order)
    }
}

/// Decompose a linear plane index into (z, c, t) given a dimension order.
fn decompose_plane(index: u32, sz: u32, sc: u32, st: u32, order: DimensionOrder) -> (u32, u32, u32) {
    match order {
        DimensionOrder::XYZCT => { let z = index % sz; let c = (index / sz) % sc; let t = index / (sz * sc); (z, c, t) }
        DimensionOrder::XYZTC => { let z = index % sz; let t = (index / sz) % st; let c = index / (sz * st); (z, c, t) }
        DimensionOrder::XYCZT => { let c = index % sc; let z = (index / sc) % sz; let t = index / (sc * sz); (z, c, t) }
        DimensionOrder::XYCTZ => { let c = index % sc; let t = (index / sc) % st; let z = index / (sc * st); (z, c, t) }
        DimensionOrder::XYTCZ => { let t = index % st; let c = (index / st) % sc; let z = index / (st * sc); (z, c, t) }
        DimensionOrder::XYTZC => { let t = index % st; let z = (index / st) % sz; let c = index / (st * sz); (z, c, t) }
    }
}

/// Compose (z, c, t) into a linear plane index given a dimension order.
fn compose_plane(z: u32, c: u32, t: u32, sz: u32, sc: u32, st: u32, order: DimensionOrder) -> u32 {
    match order {
        DimensionOrder::XYZCT => t * sz * sc + c * sz + z,
        DimensionOrder::XYZTC => c * sz * st + t * sz + z,
        DimensionOrder::XYCZT => t * sc * sz + z * sc + c,
        DimensionOrder::XYCTZ => z * sc * st + t * sc + c,
        DimensionOrder::XYTCZ => z * st * sc + c * st + t,
        DimensionOrder::XYTZC => c * st * sz + z * st + t,
    }
}

impl FormatReader for DimensionSwapper {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize { self.inner.series_count() }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)?;
        self.rebuild_meta();
        Ok(())
    }

    fn series(&self) -> usize { self.inner.series() }

    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta.as_ref().unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(self.remap_plane(plane_index))
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(self.remap_plane(plane_index), x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(self.remap_plane(plane_index))
    }

    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
    fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
}

// ── MinMaxCalculator ────────────────────────────────────────────────────────

/// Computes per-channel min/max pixel values, caching results after first read.
///
/// After reading a plane, the min/max values for each channel are available
/// via `channel_min_max()`. Values are lazily computed — only planes that have
/// been read contribute to the statistics.
pub struct MinMaxCalculator {
    inner: Box<dyn FormatReader>,
    /// Per-channel (min, max) as f64. Updated on each `open_bytes` call.
    channel_stats: Vec<(f64, f64)>,
}

impl MinMaxCalculator {
    pub fn new(inner: Box<dyn FormatReader>) -> Self {
        MinMaxCalculator { inner, channel_stats: Vec::new() }
    }

    /// Return per-channel (min, max) values computed so far.
    pub fn channel_min_max(&self) -> &[(f64, f64)] {
        &self.channel_stats
    }

    fn update_stats(&mut self, data: &[u8]) {
        let meta = self.inner.metadata();
        let nc = meta.size_c as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let pt = meta.pixel_type;
        let interleaved = meta.is_interleaved && meta.is_rgb;

        while self.channel_stats.len() < nc {
            self.channel_stats.push((f64::INFINITY, f64::NEG_INFINITY));
        }

        if interleaved && bps > 0 {
            let pixel_bytes = nc * bps;
            let n_pixels = data.len() / pixel_bytes;
            for i in 0..n_pixels {
                for c in 0..nc {
                    let offset = i * pixel_bytes + c * bps;
                    let val = read_sample(data, offset, pt);
                    let (ref mut mn, ref mut mx) = self.channel_stats[c];
                    if val < *mn { *mn = val; }
                    if val > *mx { *mx = val; }
                }
            }
        } else if bps > 0 {
            let n_samples = data.len() / bps;
            let (ref mut mn, ref mut mx) = self.channel_stats[0];
            for i in 0..n_samples {
                let val = read_sample(data, i * bps, pt);
                if val < *mn { *mn = val; }
                if val > *mx { *mx = val; }
            }
        }
    }
}

fn read_sample(data: &[u8], offset: usize, pt: PixelType) -> f64 {
    match pt {
        PixelType::Uint8 => data[offset] as f64,
        PixelType::Int8 => data[offset] as i8 as f64,
        PixelType::Uint16 => {
            u16::from_le_bytes([data[offset], data[offset + 1]]) as f64
        }
        PixelType::Int16 => {
            i16::from_le_bytes([data[offset], data[offset + 1]]) as f64
        }
        PixelType::Uint32 => {
            u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as f64
        }
        PixelType::Int32 => {
            i32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as f64
        }
        PixelType::Float32 => {
            f32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as f64
        }
        PixelType::Float64 => {
            f64::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3],
                data[offset+4], data[offset+5], data[offset+6], data[offset+7]])
        }
        PixelType::Bit => if data[offset] != 0 { 1.0 } else { 0.0 },
    }
}

impl FormatReader for MinMaxCalculator {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.channel_stats.clear();
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.channel_stats.clear();
        self.inner.close()
    }

    fn series_count(&self) -> usize { self.inner.series_count() }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.channel_stats.clear();
        self.inner.set_series(series)
    }

    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes(plane_index)?;
        self.update_stats(&data);
        Ok(data)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes_region(plane_index, x, y, w, h)?;
        self.update_stats(&data);
        Ok(data)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
    fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
}

// ── ChannelFiller ───────────────────────────────────────────────────────────

/// Fills missing channel data when a format reports more channels than it
/// actually provides pixel data for.
///
/// If an image claims `size_c = 3` but each plane only contains data for fewer
/// channels, ChannelFiller pads the output with zeros for the missing channels.
pub struct ChannelFiller {
    inner: Box<dyn FormatReader>,
    fill_to: Option<u32>,
    adjusted_meta: Option<ImageMetadata>,
}

impl ChannelFiller {
    pub fn new(inner: Box<dyn FormatReader>) -> Self {
        ChannelFiller { inner, fill_to: None, adjusted_meta: None }
    }

    /// Force output to have exactly `n` interleaved channels, zero-padding extras.
    pub fn with_channels(mut self, n: u32) -> Self {
        self.fill_to = Some(n);
        self
    }

    fn rebuild_meta(&mut self) {
        if let Some(target_c) = self.fill_to {
            let meta = self.inner.metadata();
            if target_c != meta.size_c {
                let mut adjusted = meta.clone();
                adjusted.size_c = target_c;
                adjusted.is_rgb = target_c >= 3;
                self.adjusted_meta = Some(adjusted);
                return;
            }
        }
        self.adjusted_meta = None;
    }

    fn fill_data(&self, data: Vec<u8>, target_c: u32) -> Vec<u8> {
        let meta = self.inner.metadata();
        let actual_c = meta.size_c as usize;
        let target = target_c as usize;
        if actual_c >= target { return data; }
        let bps = meta.pixel_type.bytes_per_sample();
        if bps == 0 || !meta.is_interleaved { return data; }
        let src_pixel = actual_c * bps;
        let dst_pixel = target * bps;
        let n_pixels = data.len() / src_pixel;
        let mut out = vec![0u8; n_pixels * dst_pixel];
        for i in 0..n_pixels {
            out[i * dst_pixel..i * dst_pixel + src_pixel]
                .copy_from_slice(&data[i * src_pixel..i * src_pixel + src_pixel]);
        }
        out
    }
}

impl FormatReader for ChannelFiller {
    fn is_this_type_by_name(&self, path: &Path) -> bool { self.inner.is_this_type_by_name(path) }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { self.inner.is_this_type_by_bytes(header) }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path)?; self.rebuild_meta(); Ok(()) }
    fn close(&mut self) -> Result<()> { self.adjusted_meta = None; self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s)?; self.rebuild_meta(); Ok(()) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.adjusted_meta.as_ref().unwrap_or_else(|| self.inner.metadata()) }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes(p)?;
        Ok(if let Some(c) = self.fill_to { self.fill_data(data, c) } else { data })
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(if let Some(c) = self.fill_to { self.fill_data(data, c) } else { data })
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    fn resolution(&self) -> usize { self.inner.resolution() }
    fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
}
