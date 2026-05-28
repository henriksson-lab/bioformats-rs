//! Reader wrappers that transform pixel data or metadata on the fly.
//!
//! Equivalent to Java Bio-Formats' `ReaderWrapper` hierarchy:
//! `ChannelSeparator`, `ChannelMerger`, `ChannelFiller`, `DimensionSwapper`,
//! `MinMaxCalculator`.

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::ome_metadata::{OmeChannel, OmeMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use std::path::Path;

fn wrapper_ome_metadata(
    inner: &dyn FormatReader,
    adjusted_meta: Option<&ImageMetadata>,
) -> Option<OmeMetadata> {
    let mut ome = inner.ome_metadata()?;
    if let Some(meta) = adjusted_meta {
        let image_index = inner.series();
        let image_index = if image_index < ome.images.len() {
            image_index
        } else {
            0
        };
        let target = ome.images.get_mut(image_index)?;
        let existing_channels = target.channels.clone();
        let samples_per_pixel = if meta.is_rgb { meta.size_c } else { 1 };
        target.channels = (0..meta.size_c)
            .map(|i| OmeChannel {
                name: existing_channels
                    .get(i as usize)
                    .and_then(|ch| ch.name.clone()),
                samples_per_pixel,
                color: existing_channels.get(i as usize).and_then(|ch| ch.color),
                emission_wavelength: existing_channels
                    .get(i as usize)
                    .and_then(|ch| ch.emission_wavelength),
                excitation_wavelength: existing_channels
                    .get(i as usize)
                    .and_then(|ch| ch.excitation_wavelength),
            })
            .collect();
    }
    Some(ome)
}

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
        ChannelSeparator {
            inner,
            adjusted_meta: None,
        }
    }

    /// Java ChannelSeparator separates whenever `reader.isRGB() && !reader.isIndexed()`,
    /// regardless of interleaving. In this metadata model `getRGBChannelCount()`
    /// equals `size_c` when RGB (effectiveSizeC == 1), so the split factor is
    /// `size_c`.
    fn should_split(meta: &ImageMetadata) -> bool {
        meta.is_rgb && !meta.is_indexed && meta.size_c > 1
    }

    fn rebuild_meta(&mut self) {
        let meta = self.inner.metadata();
        if Self::should_split(meta) {
            let mut adjusted = meta.clone();
            // Java getImageCount() = getRGBChannelCount() * reader.getImageCount().
            adjusted.image_count = meta.image_count * meta.size_c;
            adjusted.is_interleaved = false;
            adjusted.is_rgb = false;
            self.adjusted_meta = Some(adjusted);
        } else {
            self.adjusted_meta = None;
        }
    }

    /// Extract one channel using `ImageTools.splitChannels` semantics, handling
    /// both interleaved (RGBRGB…) and planar (RRR…GGG…BBB…) source layouts.
    ///
    /// `n_channels` is the RGB channel count (split factor), `bps` is bytes per
    /// sample. For planar data the channel is one contiguous block of length
    /// `data.len() / n_channels`; for interleaved data the channel's samples are
    /// strided by `n_channels * bps`.
    fn extract_channel(
        data: &[u8],
        channel: usize,
        n_channels: usize,
        bps: usize,
        interleaved: bool,
    ) -> Vec<u8> {
        if n_channels <= 1 {
            return data.to_vec();
        }
        let channel_length = data.len() / n_channels;
        if !interleaved {
            // Planar: contiguous block at channel_length * channel.
            let start = channel_length * channel;
            let end = (start + channel_length).min(data.len());
            if start >= data.len() {
                return vec![0u8; channel_length];
            }
            let mut out = data[start..end].to_vec();
            out.resize(channel_length, 0);
            out
        } else {
            // Interleaved: pick `bps` bytes every `n_channels * bps` bytes.
            let stride = n_channels * bps;
            let mut out = Vec::with_capacity(channel_length);
            let mut i = 0;
            while i < data.len() {
                for k in 0..bps {
                    let src = i + channel * bps + k;
                    if out.len() < channel_length && src < data.len() {
                        out.push(data[src]);
                    }
                }
                i += stride;
            }
            out.resize(channel_length, 0);
            out
        }
    }
}

impl FormatReader for ChannelSeparator {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.inner.is_this_type_by_name(path)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)?;
        self.rebuild_meta();
        Ok(())
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta
            .as_ref()
            .unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_split, nc, bps, interleaved) = {
            let meta = self.inner.metadata();
            (
                Self::should_split(meta),
                meta.size_c,
                meta.pixel_type.bytes_per_sample(),
                meta.is_interleaved,
            )
        };
        if is_split {
            let real_plane = plane_index / nc;
            let channel = (plane_index % nc) as usize;
            let data = self.inner.open_bytes(real_plane)?;
            Ok(Self::extract_channel(
                &data,
                channel,
                nc as usize,
                bps,
                interleaved,
            ))
        } else {
            self.inner.open_bytes(plane_index)
        }
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let (is_split, nc, bps, interleaved) = {
            let meta = self.inner.metadata();
            (
                Self::should_split(meta),
                meta.size_c,
                meta.pixel_type.bytes_per_sample(),
                meta.is_interleaved,
            )
        };
        if is_split {
            let real_plane = plane_index / nc;
            let channel = (plane_index % nc) as usize;
            let data = self.inner.open_bytes_region(real_plane, x, y, w, h)?;
            Ok(Self::extract_channel(
                &data,
                channel,
                nc as usize,
                bps,
                interleaved,
            ))
        } else {
            self.inner.open_bytes_region(plane_index, x, y, w, h)
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_split, nc, bps, interleaved) = {
            let meta = self.inner.metadata();
            (
                Self::should_split(meta),
                meta.size_c,
                meta.pixel_type.bytes_per_sample(),
                meta.is_interleaved,
            )
        };
        if is_split {
            let real_plane = plane_index / nc;
            let channel = (plane_index % nc) as usize;
            let data = self.inner.open_thumb_bytes(real_plane)?;
            Ok(Self::extract_channel(
                &data,
                channel,
                nc as usize,
                bps,
                interleaved,
            ))
        } else {
            self.inner.open_thumb_bytes(plane_index)
        }
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        wrapper_ome_metadata(self.inner.as_ref(), self.adjusted_meta.as_ref())
    }
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
        ChannelMerger {
            inner,
            adjusted_meta: None,
        }
    }

    /// Java ChannelMerger.canMerge(): `c > 1 && c <= 4 && !reader.isRGB()`.
    fn can_merge(meta: &ImageMetadata) -> bool {
        meta.size_c > 1 && meta.size_c <= 4 && !meta.is_rgb
    }

    fn rebuild_meta(&mut self) -> Result<()> {
        let meta = self.inner.metadata();
        if Self::can_merge(meta) {
            if meta.image_count % meta.size_c != 0 {
                return Err(BioFormatsError::InvalidData(format!(
                    "cannot merge {} planes into {} channels",
                    meta.image_count, meta.size_c
                )));
            }
            let expected_count = meta
                .size_z
                .checked_mul(meta.size_c)
                .and_then(|v| v.checked_mul(meta.size_t))
                .ok_or_else(|| BioFormatsError::InvalidData("Z/C/T plane count overflow".into()))?;
            if expected_count != meta.image_count {
                return Err(BioFormatsError::InvalidData(format!(
                    "metadata Z/C/T plane count {expected_count} does not match image count {}",
                    meta.image_count
                )));
            }
            let mut adjusted = meta.clone();
            // Java: getImageCount() divides by getSizeC() when canMerge().
            adjusted.image_count = meta.image_count / meta.size_c;
            // Java isRGB() returns true and isInterleaved() returns false when merging.
            adjusted.is_rgb = true;
            adjusted.is_interleaved = false;
            self.adjusted_meta = Some(adjusted);
        } else {
            self.adjusted_meta = None;
        }
        Ok(())
    }

    /// Concatenate N channel buffers into one planar/contiguous buffer.
    ///
    /// Java ChannelMerger.openBytes copies each channel as a contiguous block:
    /// `System.arraycopy(b, 0, buf, c * b.length, b.length)`. The result is
    /// non-interleaved (channel 0 bytes, then channel 1 bytes, ...).
    fn concatenate(channels: &[Vec<u8>]) -> Vec<u8> {
        let total: usize = channels.iter().map(|c| c.len()).sum();
        let mut out = Vec::with_capacity(total);
        for ch in channels {
            out.extend_from_slice(ch);
        }
        out
    }

    fn source_plane_for_channel(&self, plane_index: u32, channel: u32) -> Result<u32> {
        let meta = self.inner.metadata();
        let target_count = meta
            .size_z
            .checked_mul(meta.size_t)
            .ok_or_else(|| BioFormatsError::InvalidData("Z/T plane count overflow".into()))?;
        if plane_index >= target_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if channel >= meta.size_c {
            return Err(BioFormatsError::PlaneOutOfRange(channel));
        }

        let (z, t) = decompose_plane_without_channel(
            plane_index,
            meta.size_z,
            meta.size_t,
            meta.dimension_order,
        );
        let source = compose_plane(
            z,
            channel,
            t,
            meta.size_z,
            meta.size_c,
            meta.size_t,
            meta.dimension_order,
        );
        if source >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(source));
        }
        Ok(source)
    }
}

impl FormatReader for ChannelMerger {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.inner.is_this_type_by_name(path)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta()
    }

    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)?;
        self.rebuild_meta()
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta
            .as_ref()
            .unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_merge, nc) = {
            let meta = self.inner.metadata();
            (self.adjusted_meta.is_some(), meta.size_c)
        };
        if is_merge {
            let mut channels = Vec::with_capacity(nc as usize);
            for c in 0..nc {
                let source = self.source_plane_for_channel(plane_index, c)?;
                channels.push(self.inner.open_bytes(source)?);
            }
            Ok(Self::concatenate(&channels))
        } else {
            self.inner.open_bytes(plane_index)
        }
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let (is_merge, nc) = {
            let meta = self.inner.metadata();
            (self.adjusted_meta.is_some(), meta.size_c)
        };
        if is_merge {
            let mut channels = Vec::with_capacity(nc as usize);
            for c in 0..nc {
                let source = self.source_plane_for_channel(plane_index, c)?;
                channels.push(self.inner.open_bytes_region(source, x, y, w, h)?);
            }
            Ok(Self::concatenate(&channels))
        } else {
            self.inner.open_bytes_region(plane_index, x, y, w, h)
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (is_merge, nc) = {
            let meta = self.inner.metadata();
            (self.adjusted_meta.is_some(), meta.size_c)
        };
        if is_merge {
            let mut channels = Vec::with_capacity(nc as usize);
            for c in 0..nc {
                let source = self.source_plane_for_channel(plane_index, c)?;
                channels.push(self.inner.open_thumb_bytes(source)?);
            }
            Ok(Self::concatenate(&channels))
        } else {
            self.inner.open_thumb_bytes(plane_index)
        }
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        wrapper_ome_metadata(self.inner.as_ref(), self.adjusted_meta.as_ref())
    }
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
        DimensionSwapper {
            inner,
            target_order,
            adjusted_meta: None,
        }
    }

    fn rebuild_meta(&mut self) {
        let meta = self.inner.metadata();
        let mut adjusted = meta.clone();
        adjusted.dimension_order = self.target_order;
        self.adjusted_meta = Some(adjusted);
    }

    /// Convert a linear plane index from the target dimension order to the
    /// source dimension order.
    fn remap_plane(&self, plane_index: u32) -> Result<u32> {
        let meta = self.inner.metadata();
        let (sz, sc, st) = (meta.size_z, meta.size_c, meta.size_t);
        if sz == 0 || sc == 0 || st == 0 {
            return Err(BioFormatsError::InvalidData(format!(
                "zero dimension in Z/C/T sizes: {sz}/{sc}/{st}"
            )));
        }

        let plane_count = sz
            .checked_mul(sc)
            .and_then(|v| v.checked_mul(st))
            .ok_or_else(|| BioFormatsError::InvalidData("Z/C/T plane count overflow".into()))?;
        if plane_index >= plane_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Decompose plane_index according to target order
        let (z, c, t) = decompose_plane(plane_index, sz, sc, st, self.target_order);
        // Recompose according to source order
        Ok(compose_plane(z, c, t, sz, sc, st, meta.dimension_order))
    }
}

/// Decompose a linear plane index into (z, c, t) given a dimension order.
fn decompose_plane(
    index: u32,
    sz: u32,
    sc: u32,
    st: u32,
    order: DimensionOrder,
) -> (u32, u32, u32) {
    match order {
        DimensionOrder::XYZCT => {
            let z = index % sz;
            let c = (index / sz) % sc;
            let t = index / (sz * sc);
            (z, c, t)
        }
        DimensionOrder::XYZTC => {
            let z = index % sz;
            let t = (index / sz) % st;
            let c = index / (sz * st);
            (z, c, t)
        }
        DimensionOrder::XYCZT => {
            let c = index % sc;
            let z = (index / sc) % sz;
            let t = index / (sc * sz);
            (z, c, t)
        }
        DimensionOrder::XYCTZ => {
            let c = index % sc;
            let t = (index / sc) % st;
            let z = index / (sc * st);
            (z, c, t)
        }
        DimensionOrder::XYTCZ => {
            let t = index % st;
            let c = (index / st) % sc;
            let z = index / (st * sc);
            (z, c, t)
        }
        DimensionOrder::XYTZC => {
            let t = index % st;
            let z = (index / st) % sz;
            let c = index / (st * sz);
            (z, c, t)
        }
    }
}

fn decompose_plane_without_channel(
    index: u32,
    sz: u32,
    st: u32,
    order: DimensionOrder,
) -> (u32, u32) {
    match order {
        DimensionOrder::XYZCT | DimensionOrder::XYZTC | DimensionOrder::XYCZT => {
            let z = index % sz;
            let t = index / sz;
            (z, t)
        }
        DimensionOrder::XYCTZ | DimensionOrder::XYTCZ | DimensionOrder::XYTZC => {
            let t = index % st;
            let z = index / st;
            (z, t)
        }
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
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.inner.is_this_type_by_name(path)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)?;
        self.rebuild_meta();
        Ok(())
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta
            .as_ref()
            .unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let remapped = self.remap_plane(plane_index)?;
        self.inner.open_bytes(remapped)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let remapped = self.remap_plane(plane_index)?;
        self.inner.open_bytes_region(remapped, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let remapped = self.remap_plane(plane_index)?;
        self.inner.open_thumb_bytes(remapped)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        wrapper_ome_metadata(self.inner.as_ref(), self.adjusted_meta.as_ref())
    }
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
        MinMaxCalculator {
            inner,
            channel_stats: Vec::new(),
        }
    }

    /// Return per-channel (min, max) values computed so far.
    pub fn channel_min_max(&self) -> &[(f64, f64)] {
        &self.channel_stats
    }

    /// Update per-channel min/max statistics for one plane.
    ///
    /// Ports `MinMaxCalculator.updateMinMax`. The plane's channel coordinate
    /// selects which `chanMin/chanMax` slots to update:
    /// `cBase = getZCTCoords(no)[1] * numRGB`. Each of the `numRGB` samples in
    /// the plane updates `chanMax[cBase + c]`. `numRGB` is the RGB channel count
    /// (`size_c` when RGB, else 1); the per-channel stat array therefore has
    /// `effectiveSizeC * numRGB == size_c` slots. Sample addressing follows Java:
    /// `idx = bpp * (interleaved ? i*numRGB + c : c*pixels + i)`.
    fn update_stats(&mut self, plane_index: u32, data: &[u8]) {
        let meta = self.inner.metadata();
        let bps = meta.pixel_type.bytes_per_sample();
        if bps == 0 {
            return;
        }
        let pt = meta.pixel_type;
        let little_endian = meta.is_little_endian;
        // numRGB = getRGBChannelCount(); effectiveSizeC = size_c / numRGB.
        let num_rgb = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        let total_channels = meta.size_c.max(1) as usize;
        let interleaved = meta.is_interleaved && meta.is_rgb;

        // Determine the plane's C coordinate (Java getZCTCoords(no)[1]).
        let (sz, sc, st) = (
            meta.size_z.max(1),
            // effectiveSizeC: the number of distinct C planes.
            (total_channels / num_rgb).max(1) as u32,
            meta.size_t.max(1),
        );
        let plane_count = (sz as usize) * (sc as usize) * (st as usize);
        let c_coord = if (plane_index as usize) < plane_count {
            let (_z, c, _t) = decompose_plane(plane_index, sz, sc, st, meta.dimension_order);
            c as usize
        } else {
            0
        };
        let c_base = c_coord * num_rgb;

        while self.channel_stats.len() < total_channels {
            self.channel_stats.push((f64::INFINITY, f64::NEG_INFINITY));
        }

        let pixels = data.len() / (bps * num_rgb).max(1);
        for i in 0..pixels {
            for c in 0..num_rgb {
                let sample = if interleaved {
                    i * num_rgb + c
                } else {
                    c * pixels + i
                };
                let offset = bps * sample;
                let val = match read_sample_checked(data, offset, pt, little_endian) {
                    Some(v) => v,
                    None => continue,
                };
                let slot = c_base + c;
                if slot < self.channel_stats.len() {
                    let (ref mut mn, ref mut mx) = self.channel_stats[slot];
                    if val < *mn {
                        *mn = val;
                    }
                    if val > *mx {
                        *mx = val;
                    }
                }
            }
        }
    }
}

/// Decode one sample at `offset`, returning `None` if the buffer is too short
/// to hold a full sample of `pt` (rather than panicking on a truncated plane).
fn read_sample_checked(
    data: &[u8],
    offset: usize,
    pt: PixelType,
    little_endian: bool,
) -> Option<f64> {
    let n = pt.bytes_per_sample().max(1);
    let slice = data.get(offset..offset + n)?;
    let value = match pt {
        PixelType::Uint8 => slice[0] as f64,
        PixelType::Int8 => slice[0] as i8 as f64,
        PixelType::Uint16 => {
            let bytes = [slice[0], slice[1]];
            (if little_endian {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            }) as f64
        }
        PixelType::Int16 => {
            let bytes = [slice[0], slice[1]];
            (if little_endian {
                i16::from_le_bytes(bytes)
            } else {
                i16::from_be_bytes(bytes)
            }) as f64
        }
        PixelType::Uint32 => {
            let bytes = [slice[0], slice[1], slice[2], slice[3]];
            (if little_endian {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            }) as f64
        }
        PixelType::Int32 => {
            let bytes = [slice[0], slice[1], slice[2], slice[3]];
            (if little_endian {
                i32::from_le_bytes(bytes)
            } else {
                i32::from_be_bytes(bytes)
            }) as f64
        }
        PixelType::Float32 => {
            let bytes = [slice[0], slice[1], slice[2], slice[3]];
            (if little_endian {
                f32::from_le_bytes(bytes)
            } else {
                f32::from_be_bytes(bytes)
            }) as f64
        }
        PixelType::Float64 => {
            let bytes = [
                slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
            ];
            if little_endian {
                f64::from_le_bytes(bytes)
            } else {
                f64::from_be_bytes(bytes)
            }
        }
        PixelType::Bit => {
            if slice[0] != 0 {
                1.0
            } else {
                0.0
            }
        }
    };
    Some(value)
}

impl FormatReader for MinMaxCalculator {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.inner.is_this_type_by_name(path)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.channel_stats.clear();
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.channel_stats.clear();
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.channel_stats.clear();
        self.inner.set_series(series)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes(plane_index)?;
        self.update_stats(plane_index, &data);
        Ok(data)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes_region(plane_index, x, y, w, h)?;
        self.update_stats(plane_index, &data);
        Ok(data)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        wrapper_ome_metadata(self.inner.as_ref(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockReader {
        meta: ImageMetadata,
        planes: Vec<Vec<u8>>,
        ome: Option<OmeMetadata>,
    }

    impl MockReader {
        fn new(meta: ImageMetadata, planes: Vec<Vec<u8>>) -> Self {
            Self {
                meta,
                planes,
                ome: None,
            }
        }

        fn with_ome(mut self, ome: OmeMetadata) -> Self {
            self.ome = Some(ome);
            self
        }
    }

    impl FormatReader for MockReader {
        fn is_this_type_by_name(&self, _path: &Path) -> bool {
            true
        }
        fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
            true
        }
        fn set_id(&mut self, _path: &Path) -> Result<()> {
            Ok(())
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
        fn series_count(&self) -> usize {
            1
        }
        fn set_series(&mut self, _series: usize) -> Result<()> {
            Ok(())
        }
        fn series(&self) -> usize {
            0
        }
        fn metadata(&self) -> &ImageMetadata {
            &self.meta
        }

        fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
            self.planes
                .get(plane_index as usize)
                .cloned()
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
        }

        fn open_bytes_region(
            &mut self,
            plane_index: u32,
            _x: u32,
            _y: u32,
            _w: u32,
            _h: u32,
        ) -> Result<Vec<u8>> {
            self.open_bytes(plane_index)
        }

        fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
            self.open_bytes(plane_index)
        }

        fn ome_metadata(&self) -> Option<OmeMetadata> {
            self.ome.clone()
        }
    }

    #[test]
    fn minmax_calculator_uses_big_endian_multi_byte_samples() {
        let mut meta = ImageMetadata::default();
        meta.size_x = 3;
        meta.size_y = 1;
        meta.pixel_type = PixelType::Uint16;
        meta.bits_per_pixel = 16;
        meta.is_little_endian = false;

        let data = vec![0x00, 0x02, 0x01, 0x00, 0x00, 0x10];
        let inner = Box::new(MockReader::new(meta, vec![data]));
        let mut calc = MinMaxCalculator::new(inner);

        calc.open_bytes(0).expect("open_bytes");

        assert_eq!(calc.channel_min_max(), &[(2.0, 256.0)]);
    }

    #[test]
    fn dimension_swapper_rejects_zero_ztc_dimensions() {
        let mut meta = ImageMetadata::default();
        meta.size_z = 0;
        meta.size_c = 1;
        meta.size_t = 1;

        let inner = Box::new(MockReader::new(meta, vec![vec![0]]));
        let mut swapper = DimensionSwapper::new(inner, DimensionOrder::XYZTC);

        let err = swapper.open_bytes(0).expect_err("zero Z must be rejected");
        assert!(matches!(err, BioFormatsError::InvalidData(_)));
    }

    #[test]
    fn dimension_swapper_rejects_out_of_range_target_plane() {
        let mut meta = ImageMetadata::default();
        meta.size_z = 2;
        meta.size_c = 2;
        meta.size_t = 1;
        meta.dimension_order = DimensionOrder::XYZCT;
        meta.image_count = 4;

        let planes = vec![vec![0], vec![1], vec![2], vec![3]];
        let inner = Box::new(MockReader::new(meta, planes));
        let mut swapper = DimensionSwapper::new(inner, DimensionOrder::XYCZT);

        let err = swapper.open_bytes(4).expect_err("plane 4 is outside 0..4");
        assert!(matches!(err, BioFormatsError::PlaneOutOfRange(4)));
    }

    #[test]
    fn channel_merger_uses_dimension_order_for_source_planes() {
        let mut meta = ImageMetadata::default();
        meta.size_z = 2;
        meta.size_c = 2;
        meta.size_t = 1;
        meta.image_count = 4;
        meta.dimension_order = DimensionOrder::XYZCT;
        meta.is_rgb = false;
        meta.is_interleaved = false;

        let planes = vec![vec![10], vec![20], vec![30], vec![40]];
        let inner = Box::new(MockReader::new(meta, planes));
        let mut merger = ChannelMerger::new(inner);
        merger.rebuild_meta().expect("merge metadata");

        let z0 = merger.open_bytes(0).expect("z0");
        let z1 = merger.open_bytes(1).expect("z1");

        assert_eq!(z0, vec![10, 30]);
        assert_eq!(z1, vec![20, 40]);
    }

    #[test]
    fn channel_merger_rejects_non_divisible_stacks() {
        let mut meta = ImageMetadata::default();
        meta.size_z = 1;
        meta.size_c = 2;
        meta.size_t = 1;
        meta.image_count = 3;
        meta.is_rgb = false;
        meta.is_interleaved = false;

        let inner = Box::new(MockReader::new(meta, vec![vec![1], vec![2], vec![3]]));
        let mut merger = ChannelMerger::new(inner);

        let err = merger
            .rebuild_meta()
            .expect_err("non-divisible channel stack must fail");
        assert!(matches!(err, BioFormatsError::InvalidData(_)));
    }

    #[test]
    fn channel_separator_updates_ome_channel_samples() {
        let mut meta = ImageMetadata::default();
        meta.size_c = 3;
        meta.image_count = 1;
        meta.is_rgb = true;
        meta.is_interleaved = true;

        let mut ome = OmeMetadata::from_image_metadata(&meta);
        ome.images[0].channels[0].name = Some("red".into());
        let inner = Box::new(MockReader::new(meta, vec![vec![1, 2, 3]]).with_ome(ome));
        let mut separator = ChannelSeparator::new(inner);

        separator.rebuild_meta();
        let ome = separator.ome_metadata().expect("OME metadata");

        assert_eq!(separator.metadata().image_count, 3);
        assert_eq!(ome.images[0].channels.len(), 3);
        assert!(ome.images[0]
            .channels
            .iter()
            .all(|ch| ch.samples_per_pixel == 1));
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("red"));
    }

    #[test]
    fn channel_filler_updates_ome_channel_count() {
        let mut meta = ImageMetadata::default();
        meta.size_c = 1;
        meta.image_count = 1;
        meta.is_interleaved = true;

        let ome = OmeMetadata::from_image_metadata(&meta);
        let inner = Box::new(MockReader::new(meta, vec![vec![7]]).with_ome(ome));
        let mut filler = ChannelFiller::new(inner).with_channels(3);

        filler.rebuild_meta();
        let ome = filler.ome_metadata().expect("OME metadata");

        assert_eq!(filler.metadata().size_c, 3);
        assert_eq!(ome.images[0].channels.len(), 3);
        assert!(ome.images[0]
            .channels
            .iter()
            .all(|ch| ch.samples_per_pixel == 3));
    }
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
        ChannelFiller {
            inner,
            fill_to: None,
            adjusted_meta: None,
        }
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
        if actual_c >= target {
            return data;
        }
        let bps = meta.pixel_type.bytes_per_sample();
        if bps == 0 || !meta.is_interleaved {
            return data;
        }
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
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.inner.is_this_type_by_name(path)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.rebuild_meta();
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.adjusted_meta = None;
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)?;
        self.rebuild_meta();
        Ok(())
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.adjusted_meta
            .as_ref()
            .unwrap_or_else(|| self.inner.metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes(p)?;
        Ok(if let Some(c) = self.fill_to {
            self.fill_data(data, c)
        } else {
            data
        })
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let data = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(if let Some(c) = self.fill_to {
            self.fill_data(data, c)
        } else {
            data
        })
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        wrapper_ome_metadata(self.inner.as_ref(), self.adjusted_meta.as_ref())
    }
}
