// Faithful port of the Leica Microsystems Metadata (LMS) subsystem that the
// Leica XLEF reader depends on.
//
// Java reference (read-only):
//   java-bioformats/.../in/LeicaMicrosystemsMetadata/
//     LMSXmlDocument.java, LMSCollectionXmlDocument.java, XlefDocument.java,
//     XlcfDocument.java, XlifDocument.java, LMSImageXmlDocument.java,
//     Dimension.java, Channel.java, MetadataTempBuffer.java,
//     LMSMetadataExtractor.java
//
// This port mirrors the Java classes one Rust function per Java method. The
// priority is (1) the XLEF/XLIF/XLCF document graph traversal and image-node
// discovery, and (2) the dimension/channel parsing into ImageMetadata, so that
// series counts / sizes / dimension order / pixel type come from the LMS
// metadata rather than from delegate guesses.
//
// Java DOM nodes are modelled by a small in-memory tree (`XmlNode`) so that the
// node-walking helpers (getImageNode, getNodes, GetChildWithName) can mirror the
// Java traversal exactly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;

// ---------------------------------------------------------------------------
// Minimal DOM
// ---------------------------------------------------------------------------

/// A minimal XML element node, mirroring the subset of org.w3c.dom.Node that the
/// Java LMS classes use (node name, attributes, child elements, text content).
#[derive(Debug, Clone)]
pub struct XmlNode {
    pub name: String,
    pub attributes: HashMap<String, String>,
    pub children: Vec<XmlNode>,
    /// Concatenated text content of this node and its descendants (org.w3c.dom
    /// Node.getTextContent semantics).
    pub text: String,
}

impl XmlNode {
    /// Mirror of Node.getAttributes().getNamedItem(name) + getTextContent(),
    /// i.e. LMSXmlDocument.getAttr. Returns "" for missing attributes to match
    /// org.w3c.dom Element.getAttribute (which never returns null).
    pub fn get_attribute(&self, name: &str) -> String {
        self.attributes.get(name).cloned().unwrap_or_default()
    }

    /// Mirror of LMSXmlDocument.getAttr: returns Some only if the attribute node
    /// exists (used where Java distinguishes null from "").
    pub fn get_attr(&self, name: &str) -> Option<String> {
        self.attributes.get(name).cloned()
    }
}

/// Parse an XML string into a tree of `XmlNode`. This is a small, dependency-free
/// parser sufficient for Leica XML (no entity expansion beyond the common five,
/// CDATA passed through). It is the analogue of XMLTools.parseDOM +
/// documentElement.normalize().
pub fn parse_dom(xml: &str) -> Option<XmlNode> {
    let bytes = xml.as_bytes();
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root: Option<XmlNode> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            // Text content: accumulate into the innermost open node.
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            if let Some(top) = stack.last_mut() {
                let raw = &xml[start..i];
                let decoded = decode_entities(raw);
                if !decoded.trim().is_empty() {
                    top.text.push_str(&decoded);
                }
            }
            continue;
        }
        // Comments / declarations / processing instructions / CDATA: skip.
        if xml[i..].starts_with("<!--") {
            if let Some(end) = xml[i..].find("-->") {
                i += end + 3;
            } else {
                break;
            }
            continue;
        }
        if xml[i..].starts_with("<![CDATA[") {
            if let Some(end) = xml[i + 9..].find("]]>") {
                let cdata = &xml[i + 9..i + 9 + end];
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(cdata);
                }
                i += 9 + end + 3;
            } else {
                break;
            }
            continue;
        }
        if matches!(bytes.get(i + 1), Some(b'?') | Some(b'!')) {
            if let Some(end) = xml[i..].find('>') {
                i += end + 1;
            } else {
                break;
            }
            continue;
        }
        // Closing tag.
        if bytes.get(i + 1) == Some(&b'/') {
            let end = match xml[i..].find('>') {
                Some(e) => i + e,
                None => break,
            };
            if let Some(node) = stack.pop() {
                let text = node.text.clone();
                if let Some(parent) = stack.last_mut() {
                    parent.text.push_str(&text);
                    parent.children.push(node);
                } else {
                    root = Some(node);
                }
            }
            i = end + 1;
            continue;
        }
        // Opening (possibly self-closing) tag.
        let mut j = i + 1;
        let mut quote = 0u8;
        while j < bytes.len() {
            let c = bytes[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner_raw = &xml[i + 1..j];
        let self_closing = inner_raw.trim_end().ends_with('/');
        let inner = inner_raw.trim_end().trim_end_matches('/');
        let (name, attributes) = parse_tag(inner);
        let node = XmlNode {
            name,
            attributes,
            children: Vec::new(),
            text: String::new(),
        };
        if self_closing {
            if let Some(parent) = stack.last_mut() {
                parent.children.push(node);
            } else {
                root = Some(node);
            }
        } else {
            stack.push(node);
        }
        i = j + 1;
    }
    // Unbalanced documents: drain the stack.
    while let Some(node) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else {
            root = Some(node);
        }
    }
    root
}

fn parse_tag(inner: &str) -> (String, HashMap<String, String>) {
    let name_end = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = inner[..name_end].to_string();
    let mut attrs = HashMap::new();
    let mut a = &inner[name_end..];
    loop {
        let a_trim = a.trim_start();
        if a_trim.is_empty() {
            break;
        }
        let Some(eq) = a_trim.find('=') else { break };
        let key = a_trim[..eq].trim().to_string();
        let rest = a_trim[eq + 1..].trim_start();
        let rb = rest.as_bytes();
        if rb.is_empty() {
            break;
        }
        if rb[0] == b'"' || rb[0] == b'\'' {
            let q = rb[0] as char;
            if let Some(close) = rest[1..].find(q) {
                let val = decode_entities(&rest[1..1 + close]);
                if !key.is_empty() {
                    attrs.insert(key, val);
                }
                a = &rest[1 + close + 1..];
            } else {
                break;
            }
        } else {
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            if !key.is_empty() {
                attrs.insert(key, decode_entities(&rest[..end]));
            }
            a = &rest[end..];
        }
    }
    (name, attrs)
}

fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// LMSXmlDocument (base helpers)
// ---------------------------------------------------------------------------

/// Mirror of LMSXmlDocument.GetChildWithName: returns the first direct child
/// element with the given node name.
pub fn get_child_with_name<'a>(node: &'a XmlNode, name: &str) -> Option<&'a XmlNode> {
    node.children.iter().find(|child| child.name == name)
}

/// Mirror of LMSXmlDocument.parseFilePath: URL-decode the reference, normalise
/// separators, resolve relative to the document directory, and normalise.
pub fn parse_file_path(dir: &Path, ref_path: &str) -> PathBuf {
    let url_decoded = url_decode(ref_path);
    // Java replaces '\\' and '/' with File.separatorChar; on every platform we
    // normalise both to the OS separator by going through PathBuf components.
    let normalised = url_decoded.replace('\\', "/");
    let joined = dir.join(&normalised);
    normalize_path(&joined)
}

/// Mirror of LMSFileReader.fileExists / LMSXmlDocument.fileExists: returns the
/// path if a file exists there (case-insensitive directory match), else None.
pub fn file_exists(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.exists() {
        return Some(path.to_path_buf());
    }
    // Case-insensitive fallback: scan the parent directory for a name match.
    let parent = path.parent()?;
    let target = path.file_name()?.to_str()?.to_ascii_lowercase();
    let entries = std::fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        if entry.file_name().to_str().map(|n| n.to_ascii_lowercase()) == Some(target.clone()) {
            return Some(entry.path());
        }
    }
    None
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Path normalization equivalent to java.nio.file.Path.normalize() (collapse
/// "." and ".." segments) without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut prefix = PathBuf::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            Prefix(_) | RootDir => prefix.push(comp.as_os_str()),
            CurDir => {}
            ParentDir => {
                if out.pop().is_none() {
                    prefix.push("..");
                }
            }
            Normal(part) => out.push(part.to_os_string()),
        }
    }
    let mut result = prefix;
    for part in out {
        result.push(part);
    }
    result
}

// ---------------------------------------------------------------------------
// Image format enum (LMSFileReader.ImageFormat)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Lof,
    Tif,
    Bmp,
    Jpeg,
    Png,
    Unknown,
}

// ---------------------------------------------------------------------------
// XlifDocument (LMSImageXmlDocument)
// ---------------------------------------------------------------------------

/// Mirror of XlifDocument: an XLIF references one or more image files (TIF/LOF/
/// JPEG/PNG/BMP) and carries the image metadata node.
pub struct XlifDocument {
    pub doc: XmlNode,
    pub dir: PathBuf,
    pub filepath: PathBuf,
    pub image_format: ImageFormat,
    pub image_paths: Vec<PathBuf>,
}

impl XlifDocument {
    /// Mirror of XlifDocument constructor: parse, init image paths, detect format.
    pub fn new(filepath: &Path) -> Option<XlifDocument> {
        let xml = std::fs::read_to_string(filepath).ok()?;
        let doc = parse_dom(&xml)?;
        let dir = filepath
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut xlif = XlifDocument {
            doc,
            dir,
            filepath: normalize_path(filepath),
            image_format: ImageFormat::Unknown,
            image_paths: Vec::new(),
        };
        xlif.init_image_paths();
        xlif.image_format = xlif.check_image_format();
        Some(xlif)
    }

    /// Mirror of XlifDocument.getImageNode: Element -> Data -> Image traversal.
    pub fn get_image_node(&self) -> Option<&XmlNode> {
        for child in &self.doc.children {
            if child.name == "Element" {
                for element_child in &child.children {
                    if element_child.name == "Data" {
                        for data_child in &element_child.children {
                            if data_child.name == "Image" {
                                return Some(data_child);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Mirror of XlifDocument.getImageName / getName.
    pub fn get_image_name(&self) -> Option<String> {
        first_descendant(&self.doc, "Element").and_then(|e| e.get_attr("Name"))
    }

    /// Mirror of XlifDocument.getTileCount: NumberOfElements of DimID==10.
    pub fn get_tile_count(&self) -> u32 {
        for dim in descendants(&self.doc, "DimensionDescription") {
            if dim.get_attribute("DimID") == "10" {
                return dim
                    .get_attribute("NumberOfElements")
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(1);
            }
        }
        1
    }

    /// Mirror of XlifDocument.isValid.
    pub fn is_valid(&self) -> bool {
        !self.image_paths.is_empty()
    }

    /// Mirror of XlifDocument.checkImageFormat.
    fn check_image_format(&self) -> ImageFormat {
        for path in &self.image_paths {
            let p = path.to_string_lossy().to_ascii_lowercase();
            if p.ends_with("tif") || p.ends_with("tiff") {
                return ImageFormat::Tif;
            } else if p.ends_with("bmp") {
                return ImageFormat::Bmp;
            } else if p.ends_with("jpeg") || p.ends_with("jpg") {
                return ImageFormat::Jpeg;
            } else if p.ends_with("png") {
                return ImageFormat::Png;
            } else if p.ends_with("lof") {
                return ImageFormat::Lof;
            }
        }
        ImageFormat::Unknown
    }

    /// Mirror of XlifDocument.initImagePaths: //Frame references (TIF/PNG/...),
    /// falling back to //Block references (LOF).
    fn init_image_paths(&mut self) {
        let mut references: Vec<HashMap<String, String>> = descendants(&self.doc, "Frame")
            .iter()
            .map(|n| n.attributes.clone())
            .collect();
        if references.is_empty() {
            references = descendants(&self.doc, "Block")
                .iter()
                .map(|n| n.attributes.clone())
                .collect();
        }
        for attrs in references {
            let file = attrs.get("File").cloned().unwrap_or_default().to_lowercase();
            let path = parse_file_path(&self.dir, &file);
            // Java uses fileExists(path) (which can return null), storing the
            // corrected path. We fall back to the parsed path when missing so a
            // tile-count graph can still be built for metadata-only opening.
            let corrected = file_exists(&path).unwrap_or(path);
            self.image_paths.push(corrected);
        }
    }
}

// ---------------------------------------------------------------------------
// XlcfDocument / XlefDocument (LMSCollectionXmlDocument)
// ---------------------------------------------------------------------------

/// Children of a collection document (XLEF/XLCF): either a leaf XLIF image
/// document or a nested XLCF collection.
pub enum CollectionChild {
    Xlif(XlifDocument),
    Xlcf(Box<XlcfDocument>),
}

/// Mirror of LMSCollectionXmlDocument: a document referencing other LMS xml docs.
pub struct XlcfDocument {
    pub filepath: PathBuf,
    pub dir: PathBuf,
    pub children: Vec<CollectionChild>,
}

impl XlcfDocument {
    /// Construct from a filepath, parsing references into children.
    pub fn new(filepath: &Path) -> Option<XlcfDocument> {
        let xml = std::fs::read_to_string(filepath).ok()?;
        let doc = parse_dom(&xml)?;
        let dir = filepath.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut collection = XlcfDocument {
            filepath: normalize_path(filepath),
            dir,
            children: Vec::new(),
        };
        collection.init_children(&doc);
        Some(collection)
    }

    /// Mirror of LMSCollectionXmlDocument.initChildren: add referenced xlcfs and
    /// valid xlifs as children.
    fn init_children(&mut self, doc: &XmlNode) {
        for reference in descendants(doc, "Reference") {
            let raw = reference.get_attribute("File");
            let path = parse_file_path(&self.dir, &raw);
            let corrected = match file_exists(&path) {
                Some(p) => p,
                None => continue,
            };
            let lower = corrected.to_string_lossy().to_ascii_lowercase();
            if lower.ends_with(".xlif") {
                if let Some(xlif) = XlifDocument::new(&corrected) {
                    if xlif.is_valid() {
                        self.children.push(CollectionChild::Xlif(xlif));
                    }
                }
            } else if lower.ends_with(".xlcf") {
                if let Some(xlcf) = XlcfDocument::new(&corrected) {
                    self.children.push(CollectionChild::Xlcf(Box::new(xlcf)));
                }
            }
        }
    }

    /// Mirror of LMSCollectionXmlDocument.getXlifs: returns all XLIFs referenced
    /// by this and by all referenced XLCFs.
    pub fn get_xlifs(&self) -> Vec<&XlifDocument> {
        let mut xlifs = Vec::new();
        for child in &self.children {
            match child {
                CollectionChild::Xlif(xlif) => xlifs.push(xlif),
                CollectionChild::Xlcf(xlcf) => xlifs.extend(xlcf.get_xlifs()),
            }
        }
        xlifs
    }

    /// Mirror of LMSCollectionXmlDocument.getChildrenFiles.
    pub fn get_children_files(&self, pixels: bool) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for child in &self.children {
            match child {
                CollectionChild::Xlif(xlif) => {
                    files.push(xlif.filepath.clone());
                    if pixels {
                        files.extend(xlif.image_paths.iter().cloned());
                    }
                }
                CollectionChild::Xlcf(xlcf) => {
                    files.push(xlcf.filepath.clone());
                    files.extend(xlcf.get_children_files(pixels));
                }
            }
        }
        files
    }
}

/// Mirror of XlefDocument: the top-level project document. Re-uses the XLCF
/// collection logic (XlefDocument extends LMSCollectionXmlDocument).
pub struct XlefDocument {
    pub collection: XlcfDocument,
}

impl XlefDocument {
    /// Mirror of XlefDocument constructor.
    pub fn new(filepath: &Path) -> Result<XlefDocument> {
        let collection = XlcfDocument::new(filepath).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Cannot parse Leica XLEF project {}",
                filepath.display()
            ))
        })?;
        Ok(XlefDocument { collection })
    }

    /// Mirror of LMSCollectionXmlDocument.getXlifs.
    pub fn get_xlifs(&self) -> Vec<&XlifDocument> {
        self.collection.get_xlifs()
    }

    /// Mirror of XlefDocument.getImageCount.
    pub fn get_image_count(&self) -> usize {
        self.get_xlifs()
            .iter()
            .map(|x| x.image_paths.len())
            .sum()
    }

    /// Mirror of LMSCollectionXmlDocument.getChildrenFiles.
    pub fn get_children_files(&self, pixels: bool) -> Vec<PathBuf> {
        self.collection.get_children_files(pixels)
    }
}

// ---------------------------------------------------------------------------
// Node search helpers (LMSMetadataExtractor.getNodes / getElementsByTagName)
// ---------------------------------------------------------------------------

/// Mirror of Element.getElementsByTagName: all descendant elements (any depth)
/// with the given name, in document order.
pub fn descendants<'a>(root: &'a XmlNode, name: &str) -> Vec<&'a XmlNode> {
    let mut out = Vec::new();
    collect_descendants(root, name, &mut out);
    out
}

fn collect_descendants<'a>(node: &'a XmlNode, name: &str, out: &mut Vec<&'a XmlNode>) {
    for child in &node.children {
        if child.name == name {
            out.push(child);
        }
        collect_descendants(child, name, out);
    }
}

fn first_descendant<'a>(root: &'a XmlNode, name: &str) -> Option<&'a XmlNode> {
    descendants(root, name).into_iter().next()
}

/// Like `get_nodes`, but returns each matching descendant paired with the node
/// name of its grandparent for AotfList (translateLaserLines) and its parent for
/// ATLConfocalSettingDefinition (translateDetectors). `grandparent==true` selects
/// `getParentNode().getParentNode().getNodeName()`, else `getParentNode()`.
fn get_nodes_with_parents<'a>(root: &'a XmlNode, name: &str) -> Vec<(&'a XmlNode, String)> {
    // The two callers differ in how far up they look:
    //   - AotfList -> grandparent name
    //   - ATLConfocalSettingDefinition -> parent name
    // We disambiguate by the requested name here to keep one helper.
    let grandparent = name == "AotfList";
    let mut out = Vec::new();
    // For a node `child` that is a direct child of `node`, `node` is its parent
    // and `parent_name` (the node above `node`) is its grandparent.
    collect_with_parents(root, name, grandparent, "", &mut out);
    out
}

fn collect_with_parents<'a>(
    node: &'a XmlNode,
    name: &str,
    grandparent: bool,
    node_parent_name: &str,
    out: &mut Vec<(&'a XmlNode, String)>,
) {
    for child in &node.children {
        if child.name == name {
            // child's parent is `node`; child's grandparent is `node_parent_name`.
            let label = if grandparent {
                node_parent_name.to_string()
            } else {
                node.name.clone()
            };
            out.push((child, label));
        }
        collect_with_parents(child, name, grandparent, node.name.as_str(), out);
    }
}

/// Mirror of LMSMetadataExtractor.getChannelIndex.
fn get_channel_index(filter_setting: &XmlNode) -> i32 {
    let mut data = filter_setting.get_attribute("data");
    if data.is_empty() {
        data = filter_setting.get_attribute("Data");
    }
    let channel = if data.is_empty() {
        0
    } else {
        data.parse::<i32>().unwrap_or(0)
    };
    if channel < 0 {
        return -1;
    }
    channel - 1
}

/// Mirror of loci.common.DateTools.getMillisFromTicks: Windows FILETIME-style
/// 100-ns ticks (high/low 32-bit halves) converted to milliseconds.
fn get_millis_from_ticks(high: i64, low: i64) -> i64 {
    ((high << 32) | (low & 0xffff_ffff)) / 10000
}

/// Mirror of LMSMetadataExtractor.getNodes: returns descendants named `name`. If
/// the root has none directly, it recurses into children returning the first
/// non-empty match (Java returns null when nothing is found).
pub fn get_nodes<'a>(root: &'a XmlNode, name: &str) -> Vec<&'a XmlNode> {
    // getElementsByTagName already searches all descendants, which is what the
    // recursive Java fallback ultimately achieves.
    descendants(root, name)
}

/// Mirror of LMSMetadataExtractor.getImageDescription.
fn get_image_description<'a>(root: &'a XmlNode) -> Option<&'a XmlNode> {
    first_descendant(root, "ImageDescription")
}

/// Mirror of LMSMetadataExtractor.getChannelDescriptionNodes.
fn get_channel_description_nodes<'a>(root: &'a XmlNode) -> Vec<&'a XmlNode> {
    let Some(image_description) = get_image_description(root) else {
        return Vec::new();
    };
    let Some(channels) = first_descendant(image_description, "Channels") else {
        return Vec::new();
    };
    descendants(channels, "ChannelDescription")
}

/// Mirror of LMSMetadataExtractor.getDimensionDescriptionNodes.
fn get_dimension_description_nodes<'a>(root: &'a XmlNode) -> Vec<&'a XmlNode> {
    let Some(image_description) = get_image_description(root) else {
        return Vec::new();
    };
    let Some(dimensions) = first_descendant(image_description, "Dimensions") else {
        return Vec::new();
    };
    descendants(dimensions, "DimensionDescription")
}

// ---------------------------------------------------------------------------
// Dimension (Dimension.java)
// ---------------------------------------------------------------------------

const METER_MULTIPLY: f64 = 1_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimensionKey {
    X,
    Y,
    Z,
    T,
    C,
    S,
}

impl DimensionKey {
    /// Mirror of DimensionKey.id.
    pub fn id(&self) -> i32 {
        match self {
            DimensionKey::X => 1,
            DimensionKey::Y => 2,
            DimensionKey::Z => 3,
            DimensionKey::T => 4,
            DimensionKey::C => 5,
            DimensionKey::S => 10,
        }
    }

    /// Mirror of DimensionKey.token.
    pub fn token(&self) -> char {
        match self {
            DimensionKey::X => 'X',
            DimensionKey::Y => 'Y',
            DimensionKey::Z => 'Z',
            DimensionKey::T => 'T',
            DimensionKey::C => 'C',
            DimensionKey::S => 'S',
        }
    }

    /// Mirror of DimensionKey.with(int).
    pub fn with(id: i32) -> Option<DimensionKey> {
        match id {
            1 => Some(DimensionKey::X),
            2 => Some(DimensionKey::Y),
            3 => Some(DimensionKey::Z),
            4 => Some(DimensionKey::T),
            5 => Some(DimensionKey::C),
            10 => Some(DimensionKey::S),
            _ => None,
        }
    }
}

/// Mirror of Dimension.java.
#[derive(Debug, Clone)]
pub struct Dimension {
    pub key: Option<DimensionKey>,
    pub size: i32,
    pub bytes_inc: i64,
    pub unit: String,
    length: f64,
    off_by_one_length: f64,
    pub old_physical_size: bool,
}

impl Dimension {
    /// Mirror of the Dimension(key, size, bytesInc, unit, length, oldPhysicalSize)
    /// constructor, including setLength().
    pub fn new(
        key: Option<DimensionKey>,
        size: i32,
        bytes_inc: i64,
        unit: String,
        length: f64,
        old_physical_size: bool,
    ) -> Dimension {
        let mut dim = Dimension {
            key,
            size,
            bytes_inc,
            unit,
            length: 0.0,
            off_by_one_length: 0.0,
            old_physical_size,
        };
        dim.set_length(length);
        dim
    }

    /// Mirror of Dimension.createChannelDimension.
    pub fn create_channel_dimension(channel_number: i32, bytes_inc: i64) -> Dimension {
        Dimension {
            key: Some(DimensionKey::C),
            size: channel_number,
            bytes_inc,
            unit: String::new(),
            length: 0.0,
            off_by_one_length: 0.0,
            old_physical_size: false,
        }
    }

    /// Mirror of Dimension.setLength.
    pub fn set_length(&mut self, length: f64) {
        self.length = length;
        self.off_by_one_length = 0.0;
        if self.size > 1 {
            self.off_by_one_length = self.length / self.size as f64;
            self.length /= (self.size - 1) as f64; // length per pixel
        } else {
            self.length = 0.0;
        }
        if self.unit == "Ks" {
            self.length /= 1000.0;
            self.off_by_one_length /= 1000.0;
        } else if self.unit == "m" {
            self.length *= METER_MULTIPLY;
            self.off_by_one_length *= METER_MULTIPLY;
        }
    }

    /// Mirror of Dimension.getLength.
    pub fn get_length(&self) -> f64 {
        match self.key {
            Some(DimensionKey::X) | Some(DimensionKey::Y) => {
                if self.old_physical_size {
                    self.off_by_one_length
                } else {
                    self.length
                }
            }
            _ => self.length,
        }
    }
}

// ---------------------------------------------------------------------------
// Channel (Channel.java)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Mono,
    Red,
    Green,
    Blue,
    None,
}

/// Mirror of Channel.java.
#[derive(Debug, Clone)]
pub struct Channel {
    pub channel_tag: i32,
    pub resolution: i32,
    pub min: f64,
    pub max: f64,
    pub unit: String,
    pub lut_name: String,
    pub bytes_inc: i64,
    pub channel_type: ChannelType,
}

impl Channel {
    /// Mirror of the Channel constructor + setChannelType().
    pub fn new(
        channel_tag: i32,
        resolution: i32,
        min: f64,
        max: f64,
        unit: String,
        lut_name: String,
        bytes_inc: i64,
    ) -> Channel {
        let mut channel = Channel {
            channel_tag,
            resolution,
            min,
            max,
            unit,
            lut_name,
            bytes_inc,
            channel_type: ChannelType::None,
        };
        channel.set_channel_type();
        channel
    }

    /// Mirror of Channel.setChannelType.
    fn set_channel_type(&mut self) {
        if self.channel_tag == 0 {
            self.channel_type = ChannelType::Mono;
        } else if self.lut_name == "Red" {
            self.channel_type = ChannelType::Red;
        } else if self.lut_name == "Green" {
            self.channel_type = ChannelType::Green;
        } else if self.lut_name == "Blue" {
            self.channel_type = ChannelType::Blue;
        }
    }
}

// ---------------------------------------------------------------------------
// ROI (ROI.java)
// ---------------------------------------------------------------------------

const ROI_TEXT: i32 = 512;
const ROI_SCALE_BAR: i32 = 8192;
const ROI_POLYGON: i32 = 32;
const ROI_RECTANGLE: i32 = 16;
const ROI_LINE: i32 = 256;
const ROI_ARROW: i32 = 2;
const ROI_METER_MULTIPLY: f64 = 1_000_000.0;

/// Mirror of ROI.java. Vertices/transforms are stored in physical coordinates
/// and normalised to pixel coordinates by `normalize()`.
#[derive(Debug, Clone, Default)]
pub struct Roi {
    pub roi_type: i32,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    /// Center point of the ROI (relative to the image centre).
    pub trans_x: f64,
    pub trans_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub rotation: f64,
    pub color: i64,
    pub linewidth: i32,
    pub text: Option<String>,
    pub font_name: Option<String>,
    pub font_size: Option<String>,
    pub name: Option<String>,
    normalized: bool,
}

/// A geometry produced by `ROI.storeROI` (the OME shape branch chosen by `type`).
#[derive(Debug, Clone)]
pub enum StoredShape {
    Polygon { points: Vec<(f64, f64)> },
    Rectangle { x: f64, y: f64, width: f64, height: f64 },
    Line { x1: f64, y1: f64, x2: f64, y2: f64 },
}

impl Roi {
    /// Mirror of ROI.normalize: convert vertices/translation from metres to
    /// micrometres (pixel coordinates after the * scaling in storeROI).
    fn normalize(&mut self) {
        if self.normalized {
            return;
        }
        self.trans_x *= ROI_METER_MULTIPLY;
        self.trans_y *= ROI_METER_MULTIPLY;
        for v in self.x.iter_mut() {
            *v *= ROI_METER_MULTIPLY;
        }
        for v in self.y.iter_mut() {
            *v *= ROI_METER_MULTIPLY;
        }
        self.normalized = true;
    }

    /// Mirror of ROI.storeROI: returns the OME shape geometry plus the label text.
    /// `size_x`/`size_y` are the image pixel dimensions and `alternate_center`
    /// selects the alternate centring used when single-ROI nodes are present.
    pub fn store_roi(
        &mut self,
        size_x: i32,
        size_y: i32,
        alternate_center: bool,
    ) -> Option<StoredShape> {
        if self.text.is_none() {
            self.text = self.name.clone();
        }
        self.normalize();

        let corner_x = *self.x.first()?;
        let corner_y = *self.y.first()?;

        let center_x = (size_x / 2) - 1;
        let center_y = (size_y / 2) - 1;

        let (roi_x, roi_y) = if alternate_center {
            (self.trans_x - 2.0 * corner_x, self.trans_y - 2.0 * corner_y)
        } else {
            (center_x as f64 + self.trans_x, center_y as f64 + self.trans_y)
        };

        match self.roi_type {
            ROI_POLYGON => {
                let mut points = Vec::with_capacity(self.x.len());
                for i in 0..self.x.len() {
                    let px = self.x[i] * self.scale_x + roi_x;
                    let py = self.y.get(i).copied().unwrap_or(0.0) * self.scale_y + roi_y;
                    points.push((px, py));
                }
                Some(StoredShape::Polygon { points })
            }
            ROI_TEXT | ROI_RECTANGLE => {
                let width = 2.0 * corner_x.abs();
                let height = 2.0 * corner_y.abs();
                Some(StoredShape::Rectangle {
                    x: roi_x - corner_x.abs(),
                    y: roi_y - corner_y.abs(),
                    width,
                    height,
                })
            }
            ROI_SCALE_BAR | ROI_ARROW | ROI_LINE => {
                let x2 = roi_x + *self.x.get(1)?;
                let y2 = roi_y + *self.y.get(1)?;
                Some(StoredShape::Line {
                    x1: roi_x + self.x[0],
                    y1: roi_y + self.y[0],
                    x2,
                    y2,
                })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MetadataTempBuffer (subset relevant to dimensions/channels/order)
// MetadataTempBuffer.java
// ---------------------------------------------------------------------------

/// Subset of MetadataTempBuffer covering dimension/channel bookkeeping and
/// dimension ordering for a single image (one entry per core index in Java; we
/// process one image at a time, so this holds that single image's lists).
#[derive(Default)]
pub struct ImageBuffer {
    pub dimensions: Vec<Dimension>,
    pub channels: Vec<Channel>,
    pub physical_size_x: Option<f64>,
    pub physical_size_y: Option<f64>,
    pub z_step: Option<f64>,
    pub tile_count: i32,
    pub tile_bytes_inc: i64,
    pub inverse_rgb: bool,
    pub channel_colors: Vec<i32>,

    // --- Hardware / acquisition metadata (per-image slice of MetadataTempBuffer) ---
    pub description: Option<String>,
    pub microscope_model: Option<String>,
    pub serial_number: Option<String>,
    pub objective_model: Option<String>,
    pub immersion: Option<String>,
    pub correction: Option<String>,
    pub lens_na: Option<f64>,
    pub magnification: Option<f64>,
    pub refractive_index: Option<f64>,
    pub pinhole: Option<f64>,
    pub zoom: Option<f64>,
    pub t_step: Option<f64>,
    /// Stage / field positions (UNITS.REFERENCEFRAME for posX/Y, METER for field positions).
    pub pos_x: Option<f64>,
    pub pos_y: Option<f64>,
    pub pos_z: Option<f64>,
    pub flip_x: bool,
    pub flip_y: bool,
    pub swap_xy: bool,
    pub field_pos_x: Vec<Option<f64>>,
    pub field_pos_y: Vec<Option<f64>>,
    /// Per-channel arrays (length getEffectiveSizeC).
    pub exp_times: Vec<Option<f64>>,
    pub gains: Vec<Option<f64>>,
    pub detector_offsets: Vec<Option<f64>>,
    pub channel_names: Vec<Option<String>>,
    pub ex_waves: Vec<Option<f64>>,
    /// Detector models / active flags / index->name map.
    pub detector_models: Vec<String>,
    pub active_detector: Vec<bool>,
    pub detector_indexes: HashMap<i32, String>,
    /// Filter transmittance ranges (nanometres) and models.
    pub cut_ins: Vec<f64>,
    pub cut_outs: Vec<f64>,
    pub filter_models: Vec<String>,
    /// Laser lines.
    pub laser_wavelength: Vec<f64>,
    pub laser_intensity: Vec<f64>,
    pub laser_active: Vec<bool>,
    pub laser_frap: Vec<bool>,
    /// Timestamps (seconds) and acquired date (seconds since COBOL epoch).
    pub timestamps: Vec<Option<f64>>,
    pub acquired_date: f64,
    /// ROIs and the alternate-center flag (set when a "ROI" node tree exists).
    pub image_rois: Vec<Roi>,
    pub alternate_center: bool,
}

impl ImageBuffer {
    pub fn new() -> ImageBuffer {
        ImageBuffer {
            tile_count: 1,
            ..Default::default()
        }
    }

    /// Mirror of MetadataTempBuffer.getDimension.
    pub fn get_dimension(&self, key: DimensionKey) -> Option<&Dimension> {
        self.dimensions.iter().find(|d| d.key == Some(key))
    }

    fn get_dimension_mut(&mut self, key: DimensionKey) -> Option<&mut Dimension> {
        self.dimensions.iter_mut().find(|d| d.key == Some(key))
    }

    /// Mirror of MetadataTempBuffer.addDimension.
    pub fn add_dimension(&mut self, dimension: Dimension) {
        match dimension.key {
            Some(DimensionKey::X) => self.physical_size_x = Some(dimension.get_length()),
            Some(DimensionKey::Y) => self.physical_size_y = Some(dimension.get_length()),
            Some(DimensionKey::Z) => {
                if self.z_step.is_none() {
                    self.z_step = Some(dimension.get_length().abs());
                }
            }
            Some(DimensionKey::S) => {
                self.tile_count *= dimension.size;
                self.tile_bytes_inc = dimension.bytes_inc;
            }
            _ => {}
        }
        self.dimensions.push(dimension);
    }

    /// Mirror of MetadataTempBuffer.sortDimensions.
    fn sort_dimensions(&mut self) {
        self.dimensions
            .sort_by(|a, b| a.bytes_inc.cmp(&b.bytes_inc));

        // Move X and Y to the start in bytesInc order.
        let x_index = self.dimensions.iter().position(|d| d.key == Some(DimensionKey::X));
        let y_index = self.dimensions.iter().position(|d| d.key == Some(DimensionKey::Y));
        if let (Some(_), Some(_)) = (x_index, y_index) {
            // Remove highest index first to keep the other index valid.
            let (xi, yi) = (x_index.unwrap(), y_index.unwrap());
            let (first, second) = if xi > yi { (xi, yi) } else { (yi, xi) };
            let dim_first = self.dimensions.remove(first);
            let dim_second = self.dimensions.remove(second);
            // dim_x / dim_y reconstructed by key.
            let (dim_x, dim_y) = if dim_first.key == Some(DimensionKey::X) {
                (dim_first, dim_second)
            } else {
                (dim_second, dim_first)
            };
            if dim_x.bytes_inc < dim_y.bytes_inc {
                self.dimensions.insert(0, dim_x);
                self.dimensions.insert(1, dim_y);
            } else {
                self.dimensions.insert(0, dim_y);
                self.dimensions.insert(1, dim_x);
            }
        }

        // Move dimension S to the end.
        if let Some(s_index) = self.dimensions.iter().position(|d| d.key == Some(DimensionKey::S)) {
            let dim_s = self.dimensions.remove(s_index);
            self.dimensions.push(dim_s);
        }
    }

    /// Mirror of MetadataTempBuffer.getDimensionOrder.
    pub fn get_dimension_order(&mut self) -> String {
        self.sort_dimensions();
        let standard = [
            DimensionKey::X,
            DimensionKey::Y,
            DimensionKey::Z,
            DimensionKey::C,
            DimensionKey::T,
        ];
        let mut order = String::new();
        for dimension in &self.dimensions {
            if let Some(key) = dimension.key {
                if standard.contains(&key) {
                    order.push(key.token());
                }
            }
        }
        order
    }

    /// Mirror of MetadataTempBuffer.addMissingDimensions.
    pub fn add_missing_dimensions(&mut self) {
        self.dimensions
            .sort_by(|a, b| a.bytes_inc.cmp(&b.bytes_inc));
        let last = match self.dimensions.last() {
            Some(d) => (d.bytes_inc, d.old_physical_size),
            None => (0, false),
        };
        if self.get_dimension(DimensionKey::Z).is_none() {
            self.add_dimension(Dimension::new(
                Some(DimensionKey::Z),
                1,
                last.0,
                "m".to_string(),
                1.0,
                last.1,
            ));
        }
        if self.get_dimension(DimensionKey::T).is_none() {
            self.add_dimension(Dimension::new(
                Some(DimensionKey::T),
                1,
                last.0,
                "s".to_string(),
                1.0,
                last.1,
            ));
        }
        if self.get_dimension(DimensionKey::S).is_none() {
            self.add_dimension(Dimension::new(
                Some(DimensionKey::S),
                1,
                last.0,
                String::new(),
                1.0,
                last.1,
            ));
        }
    }

    /// Mirror of MetadataTempBuffer.addChannelDimension.
    pub fn add_channel_dimension(&mut self) {
        let x_bytes_inc = self
            .get_dimension(DimensionKey::X)
            .map(|d| d.bytes_inc)
            .unwrap_or(0);
        let rgb = x_bytes_inc != 0 && (x_bytes_inc % 3) == 0;
        let size_c = if rgb {
            self.channels.len() as i32 / 3
        } else {
            self.channels.len() as i32
        };
        let channel_bytes_inc = self.get_channel_dimension_bytes_inc();
        self.add_dimension(Dimension::create_channel_dimension(size_c, channel_bytes_inc));
    }

    /// Mirror of MetadataTempBuffer.getChannelDimensionBytesInc.
    fn get_channel_dimension_bytes_inc(&self) -> i64 {
        let x_bytes_inc = self
            .get_dimension(DimensionKey::X)
            .map(|d| d.bytes_inc)
            .unwrap_or(0);
        let rgb = x_bytes_inc != 0 && (x_bytes_inc % 3) == 0;
        let mut max_bytes_inc = 0i64;
        if rgb {
            for channel in &self.channels {
                if channel.channel_tag == 3 {
                    max_bytes_inc = max_bytes_inc.max(channel.bytes_inc);
                }
            }
        } else {
            for channel in &self.channels {
                max_bytes_inc = max_bytes_inc.max(channel.bytes_inc);
            }
        }
        if max_bytes_inc == 0 {
            if let Some(y_dim) = self.get_dimension(DimensionKey::Y) {
                max_bytes_inc = y_dim.bytes_inc * y_dim.size as i64;
            }
        }
        max_bytes_inc
    }
}

// ---------------------------------------------------------------------------
// LMSMetadataExtractor (dimension / channel core extraction)
// ---------------------------------------------------------------------------

/// Core dimensions extracted for one image (subset of CoreMetadata).
pub struct CoreDimensions {
    pub size_x: u32,
    pub size_y: u32,
    pub size_z: u32,
    pub size_c: u32,
    pub size_t: u32,
    pub rgb: bool,
    pub pixel_type: PixelType,
    pub dimension_order: String,
    pub image_count: u32,
    pub interleaved: bool,
    pub indexed: bool,
}

/// Mirror of LMSMetadataExtractor (channel + dimension translation) for one
/// image node. Returns the populated buffer and computed core dimensions.
pub struct LmsMetadataExtractor {
    pub extras: i64,
    pub buffer: ImageBuffer,
    pub size_c: u32,
    /// Core sizes computed by translate_dimension_descriptions, needed by the
    /// hardware translators (getEffectiveSizeC / getImageCount / getZCTCoords).
    pub core_size_c: u32,
    pub core_size_z: u32,
    pub core_size_t: u32,
    pub core_rgb: bool,
    pub core_image_count: u32,
    pub core_dimension_order: String,
    pub image_format: ImageFormat,
}

impl LmsMetadataExtractor {
    pub fn new() -> LmsMetadataExtractor {
        LmsMetadataExtractor {
            extras: 1,
            buffer: ImageBuffer::new(),
            size_c: 0,
            core_size_c: 1,
            core_size_z: 1,
            core_size_t: 1,
            core_rgb: false,
            core_image_count: 1,
            core_dimension_order: String::new(),
            image_format: ImageFormat::Unknown,
        }
    }

    /// Mirror of LMSFileReader.getEffectiveSizeC(): the number of channel planes.
    /// FormatTools.getEffectiveSizeC = imageCount / (sizeZ * sizeT).
    pub fn get_effective_size_c(&self) -> i32 {
        let zt = (self.core_size_z * self.core_size_t).max(1);
        (self.core_image_count / zt).max(1) as i32
    }

    /// Mirror of LMSFileReader.getImageCount().
    pub fn get_image_count(&self) -> i32 {
        self.core_image_count.max(1) as i32
    }

    /// Mirror of FormatReader.isRGB().
    pub fn is_rgb(&self) -> bool {
        self.core_rgb
    }

    /// Mirror of LMSMetadataExtractor.parseLong.
    fn parse_long(value: &str) -> i64 {
        let v = value.trim();
        if v.is_empty() {
            0
        } else {
            v.parse::<i64>().unwrap_or(0)
        }
    }

    /// Mirror of LMSMetadataExtractor.parseInt.
    fn parse_int(value: &str) -> i32 {
        let v = value.trim();
        if v.is_empty() {
            0
        } else {
            v.parse::<i32>().unwrap_or(0)
        }
    }

    /// Mirror of LMSMetadataExtractor.parseDouble.
    fn parse_double(value: &str) -> f64 {
        let v = value.trim();
        if v.is_empty() {
            0.0
        } else {
            v.parse::<f64>().unwrap_or(0.0)
        }
    }

    /// Mirror of LMSMetadataExtractor.translateChannelDescriptions.
    pub fn translate_channel_descriptions(&mut self, image_node: &XmlNode) {
        let channels = get_channel_description_nodes(image_node);
        self.size_c = channels.len() as u32;

        for channel_element in &channels {
            let channel_tag = Self::parse_int(&channel_element.get_attribute("ChannelTag"));
            let resolution = Self::parse_int(&channel_element.get_attribute("Resolution"));
            let min = Self::parse_double(&channel_element.get_attribute("Min"));
            let max = Self::parse_double(&channel_element.get_attribute("Max"));
            let unit = channel_element.get_attribute("Unit");
            let lut_name = channel_element.get_attribute("LUTName");
            let bytes_inc = Self::parse_long(&channel_element.get_attribute("BytesInc"));
            let channel = Channel::new(channel_tag, resolution, min, max, unit, lut_name, bytes_inc);
            self.buffer.channels.push(channel);
        }

        // BGR order is assumed when LUT names don't explicitly describe RGB.
        self.buffer.inverse_rgb = !(channels.len() >= 3
            && channels[0].get_attribute("LUTName") == "Red"
            && channels[1].get_attribute("LUTName") == "Green"
            && channels[2].get_attribute("LUTName") == "Blue");

        let luts: Vec<String> = channels
            .iter()
            .map(|c| c.get_attribute("LUTName"))
            .collect();
        self.translate_luts(&luts);
    }

    /// Mirror of LMSMetadataExtractor.translateLuts.
    fn translate_luts(&mut self, luts: &[String]) {
        let mut colors = Vec::with_capacity(self.size_c as usize);
        let mut next_lut = 0usize;
        for _channel in 0..self.size_c {
            if next_lut < luts.len() {
                colors.push(Self::translate_lut(&luts[next_lut]));
                next_lut += 1;
            }
        }
        self.buffer.channel_colors = colors;
    }

    /// Mirror of LMSMetadataExtractor.translateLut. Returns a packed RGBA i32
    /// (ome.xml Color), matching the channel.color convention used elsewhere.
    fn translate_lut(lut_name: &str) -> i32 {
        let cleaned: String = lut_name.chars().filter(|c| !c.is_whitespace()).collect();
        let lower = cleaned.to_ascii_lowercase();
        if lower.starts_with("gradient(") && cleaned.ends_with(')') {
            let inner = &cleaned[9..cleaned.len() - 1];
            let rgb: Vec<&str> = inner.split(',').collect();
            if rgb.len() == 3 {
                let r = rgb[2].parse::<i32>().unwrap_or(255);
                let g = rgb[1].parse::<i32>().unwrap_or(255);
                let b = rgb[0].parse::<i32>().unwrap_or(255);
                return pack_color(r, g, b, 255);
            }
            return pack_color(255, 255, 255, 255);
        }
        match lower.as_str() {
            "red" => pack_color(255, 0, 0, 255),
            "green" => pack_color(0, 255, 0, 255),
            "blue" => pack_color(0, 0, 255, 255),
            "cyan" => pack_color(0, 255, 255, 255),
            "magenta" => pack_color(255, 0, 255, 255),
            "yellow" => pack_color(255, 255, 0, 255),
            _ => pack_color(255, 255, 255, 255),
        }
    }

    /// Mirror of LMSMetadataExtractor.translateDimensionDescriptions.
    /// `is_tif_or_jpeg` selects interleaving behaviour like getImageFormat().
    pub fn translate_dimension_descriptions(
        &mut self,
        image_node: &XmlNode,
        old_physical_size: bool,
        is_tif_or_jpeg: bool,
    ) -> Result<CoreDimensions> {
        let dimensions = get_dimension_description_nodes(image_node);

        for dimension_element in &dimensions {
            let id = Self::parse_int(&dimension_element.get_attribute("DimID"));
            let size = Self::parse_int(&dimension_element.get_attribute("NumberOfElements"));
            let bytes_inc = Self::parse_long(&dimension_element.get_attribute("BytesInc"));
            let length = Self::parse_double(&dimension_element.get_attribute("Length"));
            let unit = dimension_element.get_attribute("Unit");

            let key = DimensionKey::with(id);
            let dimension = Dimension::new(key, size, bytes_inc, unit, length, old_physical_size);
            let dim_size = dimension.size;
            self.buffer.add_dimension(dimension);
            if key.is_none() {
                self.extras *= dim_size as i64;
            }
        }

        self.buffer.add_channel_dimension();
        self.buffer.add_missing_dimensions();
        let mut core = self.set_core_dimension_sizes()?;
        core.pixel_type = self.set_pixel_type()?;

        core.interleaved = if is_tif_or_jpeg { false } else { core.rgb };
        core.indexed = !core.rgb;
        core.image_count = core.size_z * core.size_t;
        if !core.rgb {
            core.image_count *= core.size_c;
        } else {
            core.image_count *= core.size_c / 3;
        }
        core.dimension_order = self.buffer.get_dimension_order();

        // Record the computed core so the hardware translators (which run after
        // dimension translation in translateImage) can use getEffectiveSizeC,
        // getImageCount, isRGB and getZCTCoords.
        self.core_size_c = core.size_c;
        self.core_size_z = core.size_z;
        self.core_size_t = core.size_t;
        self.core_rgb = core.rgb;
        self.core_image_count = core.image_count.max(1);
        self.core_dimension_order = core.dimension_order.clone();
        self.image_format = if is_tif_or_jpeg {
            ImageFormat::Tif
        } else {
            self.image_format
        };
        Ok(core)
    }

    /// Mirror of LMSMetadataExtractor.setCoreDimensionSizes.
    fn set_core_dimension_sizes(&mut self) -> Result<CoreDimensions> {
        let size_x = self
            .buffer
            .get_dimension(DimensionKey::X)
            .map(|d| d.size)
            .unwrap_or(0);
        let size_y = self
            .buffer
            .get_dimension(DimensionKey::Y)
            .map(|d| d.size)
            .unwrap_or(0);
        let mut size_z = self
            .buffer
            .get_dimension(DimensionKey::Z)
            .map(|d| d.size)
            .unwrap_or(0);
        let mut size_t = self
            .buffer
            .get_dimension(DimensionKey::T)
            .map(|d| d.size)
            .unwrap_or(0);
        let x_bytes_inc = self
            .buffer
            .get_dimension(DimensionKey::X)
            .map(|d| d.bytes_inc)
            .unwrap_or(0);
        let rgb = x_bytes_inc != 0 && (x_bytes_inc % 3) == 0;
        if rgb {
            if let Some(x_dim) = self.buffer.get_dimension_mut(DimensionKey::X) {
                x_dim.bytes_inc /= 3;
            }
        }

        if self.extras > 1 {
            if size_z == 1 {
                size_z = self.extras as i32;
            } else if size_t == 0 {
                size_t = self.extras as i32;
            } else {
                size_t *= self.extras as i32;
            }
        }

        let mut size_c = self.size_c as i32;
        let size_x = if size_x == 0 { 1 } else { size_x };
        let size_y = if size_y == 0 { 1 } else { size_y };
        if size_c == 0 {
            size_c = 1;
        }
        if size_z == 0 {
            size_z = 1;
        }
        if size_t == 0 {
            size_t = 1;
        }

        Ok(CoreDimensions {
            size_x: size_x as u32,
            size_y: size_y as u32,
            size_z: size_z as u32,
            size_c: size_c as u32,
            size_t: size_t as u32,
            rgb,
            pixel_type: PixelType::Uint8,
            dimension_order: String::new(),
            image_count: 0,
            interleaved: false,
            indexed: false,
        })
    }

    /// Mirror of LMSMetadataExtractor.setPixelType (FormatTools.pixelTypeFromBytes
    /// with signed=false, allowLong=true).
    fn set_pixel_type(&self) -> Result<PixelType> {
        let x_bytes_inc = self
            .buffer
            .get_dimension(DimensionKey::X)
            .map(|d| d.bytes_inc)
            .unwrap_or(0);
        pixel_type_from_bytes(x_bytes_inc as i32)
    }

    /// Mirror of DataTools.parseDouble: returns None when the string is blank or
    /// unparsable (Java returns null), rather than the parseDouble(...)=0 helper.
    fn data_parse_double(value: &str) -> Option<f64> {
        let v = value.trim();
        if v.is_empty() {
            return None;
        }
        v.parse::<f64>().ok()
    }

    /// Mirror of LMSMetadataExtractor.translateImage's hardware/ROI section: runs
    /// the remaining translate* methods in Java order. translateChannelDescriptions
    /// and translateDimensionDescriptions must already have been run.
    pub fn translate_image_extras(&mut self, image_node: &XmlNode) {
        let effective_c = self.get_effective_size_c().max(0) as usize;
        // Allocate the per-channel arrays as Java does in translateScannerSettings.
        self.buffer.exp_times = vec![None; effective_c];
        self.buffer.gains = vec![None; effective_c];
        self.buffer.detector_offsets = vec![None; effective_c];
        self.buffer.channel_names = vec![None; effective_c];
        self.buffer.ex_waves = vec![None; effective_c];

        self.translate_attachment_nodes(image_node);
        self.translate_scanner_settings(image_node);
        self.translate_filter_settings(image_node);
        self.translate_timestamps(image_node);
        self.translate_laser_lines(image_node);
        self.translate_rois(image_node);
        self.translate_single_rois(image_node);
        self.translate_detectors(image_node);
    }

    /// Mirror of LMSMetadataExtractor.translateAttachmentNodes (the field-position
    /// / TileScanInfo / HardwareSetting stage-position extraction).
    fn translate_attachment_nodes(&mut self, image_node: &XmlNode) {
        let attachment_nodes = get_nodes(image_node, "Attachment");
        if attachment_nodes.is_empty() {
            return;
        }

        let mut tiles_attachment_found = false;
        for attachment in &attachment_nodes {
            let attachment_name = attachment.get_attribute("Name");
            if attachment_name == "TileScanInfo" {
                let tiles = get_nodes(attachment, "Tile");
                for tile_node in &tiles {
                    let pos_x = tile_node.get_attr("PosX");
                    let pos_y = tile_node.get_attr("PosY");
                    if let Some(px) = pos_x {
                        self.buffer.field_pos_x.push(Self::data_parse_double(&px));
                    }
                    if let Some(py) = pos_y {
                        self.buffer.field_pos_y.push(Self::data_parse_double(&py));
                    }
                }
                tiles_attachment_found = true;
            }
        }

        if !tiles_attachment_found {
            let mut confocal_settings: Vec<&XmlNode> = Vec::new();
            for attachment in &attachment_nodes {
                if attachment.get_attribute("Name") == "HardwareSetting" {
                    confocal_settings = get_nodes(attachment, "ATLConfocalSettingDefinition");
                    break;
                }
            }
            if !confocal_settings.is_empty() {
                for confocal_setting in &confocal_settings {
                    let value = confocal_setting.get_attribute("StagePosX");
                    if !value.trim().is_empty() {
                        self.buffer
                            .field_pos_x
                            .push(Self::data_parse_double(value.trim()));
                    }
                    let value = confocal_setting.get_attribute("StagePosY");
                    if !value.trim().is_empty() {
                        self.buffer
                            .field_pos_y
                            .push(Self::data_parse_double(value.trim()));
                    }
                }
            } else {
                self.buffer.field_pos_x.push(None);
                self.buffer.field_pos_y.push(None);
            }
        }
    }

    /// Mirror of LMSMetadataExtractor.translateScannerSettings.
    fn translate_scanner_settings(&mut self, image_node: &XmlNode) {
        let scanner_settings = get_nodes(image_node, "ScannerSettingRecord");
        let attachment_nodes = get_nodes(image_node, "Attachment");
        if attachment_nodes.is_empty() {
            return;
        }
        let mut confocal_settings: Vec<&XmlNode> = Vec::new();
        for attachment in &attachment_nodes {
            if attachment.get_attribute("Name") == "HardwareSetting" {
                confocal_settings = get_nodes(attachment, "ATLConfocalSettingDefinition");
            }
        }
        if scanner_settings.is_empty() && confocal_settings.is_empty() {
            return;
        }

        let effective_c = self.get_effective_size_c();
        for scanner_setting in &scanner_settings {
            let id = scanner_setting.get_attribute("Identifier");
            let value = scanner_setting.get_attribute("Variant");
            if value.trim().is_empty() {
                continue;
            }
            if id == "SystemType" {
                self.buffer.microscope_model = Some(value);
            } else if id == "dblPinhole" {
                self.buffer.pinhole =
                    Self::data_parse_double(value.trim()).map(|v| v * METER_MULTIPLY);
            } else if id == "dblZoom" {
                self.buffer.zoom = Self::data_parse_double(value.trim());
            } else if id == "dblStepSize" {
                self.buffer.z_step =
                    Self::data_parse_double(value.trim()).map(|v| v * METER_MULTIPLY);
            } else if id == "nDelayTime_s" {
                self.buffer.t_step = Self::data_parse_double(value.trim());
            } else if id == "CameraName" {
                self.buffer.detector_models.push(value);
            } else if id.find("WFC") == Some(1) {
                let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
                let c = digits.parse::<i32>().unwrap_or(0);
                if c < 0 || c >= effective_c {
                    continue;
                }
                let c = c as usize;
                if id.ends_with("ExposureTime") {
                    self.buffer.exp_times[c] = Self::data_parse_double(value.trim());
                } else if id.ends_with("Gain") {
                    self.buffer.gains[c] = Self::data_parse_double(value.trim());
                } else if id.ends_with("WaveLength") {
                    if let Some(ex_wave) = Self::data_parse_double(value.trim()) {
                        if ex_wave > 0.0 {
                            self.buffer.ex_waves[c] = Some(ex_wave);
                        }
                    }
                } else if (id.ends_with("UesrDefName") || id.ends_with("UserDefName"))
                    && value != "None"
                {
                    if self.buffer.channel_names[c]
                        .as_deref()
                        .map(|n| n.trim().is_empty())
                        .unwrap_or(true)
                    {
                        self.buffer.channel_names[c] = Some(value);
                    }
                }
            }
        }

        for confocal_setting in &confocal_settings {
            let value = confocal_setting.get_attribute("Pinhole");
            if !value.trim().is_empty() {
                self.buffer.pinhole =
                    Self::data_parse_double(value.trim()).map(|v| v * METER_MULTIPLY);
            }
            let value = confocal_setting.get_attribute("Zoom");
            if !value.trim().is_empty() {
                self.buffer.zoom = Self::data_parse_double(value.trim());
            }
            let value = confocal_setting.get_attribute("ObjectiveName");
            if !value.trim().is_empty() {
                self.buffer.objective_model = Some(value.trim().to_string());
            }
            let value = confocal_setting.get_attribute("FlipX");
            if !value.trim().is_empty() {
                self.buffer.flip_x = value.trim() == "1";
            }
            let value = confocal_setting.get_attribute("FlipY");
            if !value.trim().is_empty() {
                self.buffer.flip_y = value.trim() == "1";
            }
            let value = confocal_setting.get_attribute("SwapXY");
            if !value.trim().is_empty() {
                self.buffer.swap_xy = value.trim() == "1";
            }
        }
    }

    /// Mirror of LMSMetadataExtractor.translateFilterSettings.
    fn translate_filter_settings(&mut self, image_node: &XmlNode) {
        let filter_settings = get_nodes(image_node, "FilterSettingRecord");
        if filter_settings.is_empty() {
            return;
        }
        let mut next_channel = 0usize;
        for filter_setting in &filter_settings {
            let object = filter_setting.get_attribute("ObjectName");
            let attribute = filter_setting.get_attribute("Attribute");
            let object_class = filter_setting.get_attribute("ClassName");
            let variant = filter_setting.get_attribute("Variant");
            let data = filter_setting.get_attribute("Data");

            if attribute == "NumericalAperture" {
                if !variant.trim().is_empty() {
                    self.buffer.lens_na = Self::data_parse_double(variant.trim());
                }
            } else if attribute == "OrderNumber" {
                if !variant.trim().is_empty() {
                    self.buffer.serial_number = Some(variant.trim().to_string());
                }
            } else if object_class == "CDetectionUnit" {
                if attribute == "State" {
                    let channel = get_channel_index(filter_setting);
                    if channel < 0 {
                        continue;
                    }
                    if let Ok(key) = data.parse::<i32>() {
                        self.buffer.detector_indexes.insert(key, object.clone());
                    }
                    self.buffer
                        .active_detector
                        .push(variant.trim() == "Active");
                }
            } else if attribute == "Objective" {
                // Tokenise the objective string: "<mag>x<NA> <immersion> <correction>".
                let mut tokens = variant.split(' ').filter(|t| !t.is_empty());
                let mut found_mag = false;
                let mut model = String::new();
                while !found_mag {
                    let Some(token) = tokens.next() else { break };
                    if let Some(x) = token.find('x') {
                        found_mag = true;
                        let na = &token[x + 1..];
                        if !na.trim().is_empty() {
                            self.buffer.lens_na = Self::data_parse_double(na.trim());
                        }
                        let mag = &token[..x];
                        if !mag.trim().is_empty() {
                            self.buffer.magnification = Self::data_parse_double(mag.trim());
                        }
                    } else {
                        model.push_str(token);
                        model.push(' ');
                    }
                }
                let immersion = match tokens.next() {
                    Some(t) if !t.trim().is_empty() => t.to_string(),
                    _ => "Other".to_string(),
                };
                self.buffer.immersion = Some(immersion);
                let correction = match tokens.next() {
                    Some(t) if !t.trim().is_empty() => t.to_string(),
                    _ => "Other".to_string(),
                };
                self.buffer.correction = Some(correction);
                self.buffer.objective_model = Some(model.trim().to_string());
            } else if attribute == "RefractionIndex" {
                if !variant.trim().is_empty() {
                    self.buffer.refractive_index = Self::data_parse_double(variant.trim());
                }
            } else if attribute == "XPos" {
                if !variant.trim().is_empty() {
                    self.buffer.pos_x = Self::data_parse_double(variant.trim());
                }
            } else if attribute == "YPos" {
                if !variant.trim().is_empty() {
                    self.buffer.pos_y = Self::data_parse_double(variant.trim());
                }
            } else if attribute == "ZPos" {
                if !variant.trim().is_empty() {
                    self.buffer.pos_z = Self::data_parse_double(variant.trim());
                }
            } else if object_class == "CSpectrophotometerUnit" {
                let v = Self::data_parse_double(&variant);
                let description = filter_setting.get_attribute("Description");
                if description.ends_with("(left)") {
                    self.buffer.filter_models.push(object.clone());
                    if let Some(v) = v {
                        if v > 0.0 {
                            self.buffer.cut_ins.push(v);
                        }
                    }
                } else if description.ends_with("(right)") {
                    if let Some(v) = v {
                        if v > 0.0 {
                            self.buffer.cut_outs.push(v);
                        }
                    }
                } else if attribute == "Stain" {
                    if next_channel < self.buffer.channel_names.len() {
                        self.buffer.channel_names[next_channel] = Some(variant);
                        next_channel += 1;
                    }
                }
            }
        }
    }

    /// Mirror of LMSMetadataExtractor.translateTimestamps.
    fn translate_timestamps(&mut self, image_node: &XmlNode) {
        let time_stamp_lists = get_nodes(image_node, "TimeStampList");
        if time_stamp_lists.is_empty() {
            return;
        }
        let time_stamp_list = time_stamp_lists[0];
        let image_count = self.get_image_count().max(0) as usize;
        self.buffer.timestamps = vec![None; image_count];

        let number_of_time_stamps = time_stamp_list.get_attribute("NumberOfTimeStamps");
        if !number_of_time_stamps.is_empty() {
            // LAS AF 3.1 (or newer): space-separated hex timestamps in text content.
            let raw = time_stamp_list.text.clone();
            for (stamp, timestamp) in raw.split(' ').filter(|t| !t.is_empty()).enumerate() {
                if stamp < image_count {
                    self.buffer.timestamps[stamp] =
                        Some(Self::translate_single_timestamp_hex(timestamp));
                }
            }
        } else {
            // LAS AF 3.0 (or older): TimeStamp nodes with High/LowInteger.
            let timestamp_nodes = get_nodes(image_node, "TimeStamp");
            if !timestamp_nodes.is_empty() {
                for (stamp, node) in timestamp_nodes.iter().enumerate() {
                    if stamp < image_count {
                        self.buffer.timestamps[stamp] =
                            Some(Self::translate_single_timestamp_node(node));
                    }
                }
            } else {
                return;
            }
        }

        self.buffer.acquired_date = self.buffer.timestamps[0].unwrap_or(0.0);
    }

    /// Mirror of LMSMetadataExtractor.translateSingleTimestamp(String).
    fn translate_single_timestamp_hex(timestamp: &str) -> f64 {
        let timestamp = timestamp.trim();
        let stamp_low_start = timestamp.len().saturating_sub(8);
        let stamp_high_end = stamp_low_start;
        let stamp_high = &timestamp[..stamp_high_end];
        let stamp_low = &timestamp[stamp_low_start..];
        let high = if stamp_high.trim().is_empty() {
            0
        } else {
            i64::from_str_radix(stamp_high.trim(), 16).unwrap_or(0)
        };
        let low = if stamp_low.trim().is_empty() {
            0
        } else {
            i64::from_str_radix(stamp_low.trim(), 16).unwrap_or(0)
        };
        let milliseconds = get_millis_from_ticks(high, low);
        milliseconds as f64 / 1000.0
    }

    /// Mirror of LMSMetadataExtractor.translateSingleTimestamp(Element).
    fn translate_single_timestamp_node(timestamp: &XmlNode) -> f64 {
        let stamp_high = timestamp.get_attribute("HighInteger");
        let stamp_low = timestamp.get_attribute("LowInteger");
        let high = if stamp_high.trim().is_empty() {
            0
        } else {
            stamp_high.trim().parse::<i64>().unwrap_or(0)
        };
        let low = if stamp_low.trim().is_empty() {
            0
        } else {
            stamp_low.trim().parse::<i64>().unwrap_or(0)
        };
        let milliseconds = get_millis_from_ticks(high, low);
        milliseconds as f64 / 1000.0
    }

    /// Mirror of LMSMetadataExtractor.translateLaserLines.
    fn translate_laser_lines(&mut self, image_node: &XmlNode) {
        let aotf_lists = get_nodes_with_parents(image_node, "AotfList");
        if aotf_lists.is_empty() {
            return;
        }
        let mut base_intensity_index = 0usize;
        for (aotf, gp_name) in &aotf_lists {
            let laser_lines = get_nodes(aotf, "LaserLineSetting");
            if laser_lines.is_empty() {
                return;
            }
            // gpName = aotf.getParentNode().getParentNode().getNodeName().
            let is_master =
                gp_name.ends_with("Sequential_Master") || gp_name.ends_with("Attachment");
            self.buffer.laser_frap.push(gp_name.ends_with("FRAP_Master"));
            for laser_line in &laser_lines {
                if is_master {
                    continue;
                }
                let line_index = laser_line.get_attribute("LineIndex");
                let qual = laser_line.get_attribute("Qualifier");
                let index = if line_index.trim().is_empty() {
                    0
                } else {
                    line_index.trim().parse::<i32>().unwrap_or(0)
                };
                let qualifier = if qual.trim().is_empty() {
                    0
                } else {
                    qual.trim().parse::<i32>().unwrap_or(0)
                };
                let index = index + (2 - (qualifier / 10));
                if index < 0 {
                    continue;
                }
                let index = index as usize;

                let v = laser_line.get_attribute("LaserLine");
                let wavelength = if v.trim().is_empty() {
                    0.0
                } else {
                    Self::data_parse_double(v.trim()).unwrap_or(0.0)
                };
                if index < self.buffer.laser_wavelength.len() {
                    self.buffer.laser_wavelength[index] = wavelength;
                } else {
                    while self.buffer.laser_wavelength.len() < index {
                        self.buffer.laser_wavelength.push(0.0);
                    }
                    self.buffer.laser_wavelength.push(wavelength);
                }

                let intensity = laser_line.get_attribute("IntensityDev");
                let real_intensity = if intensity.trim().is_empty() {
                    0.0
                } else {
                    Self::data_parse_double(intensity.trim()).unwrap_or(0.0)
                };
                let real_intensity = 100.0 - real_intensity;

                let real_index = base_intensity_index + index;
                if real_index < self.buffer.laser_intensity.len() {
                    self.buffer.laser_intensity[real_index] = real_intensity;
                } else {
                    // Mirror the (quirky) Java loop condition, then append.
                    while real_index < self.buffer.laser_intensity.len() {
                        self.buffer.laser_intensity.push(100.0);
                    }
                    self.buffer.laser_intensity.push(real_intensity);
                }
            }
            base_intensity_index += self.buffer.laser_wavelength.len();
        }
    }

    /// Mirror of LMSMetadataExtractor.translateROIs.
    fn translate_rois(&mut self, image_node: &XmlNode) {
        let rois = get_nodes(image_node, "Annotation");
        if rois.is_empty() {
            return;
        }
        let physical_size_x = self.buffer.physical_size_x.unwrap_or(0.0);
        let physical_size_y = self.buffer.physical_size_y.unwrap_or(0.0);
        let has_roi_nodes = !get_nodes(image_node, "ROI").is_empty();

        let mut image_rois: Vec<Roi> = Vec::with_capacity(rois.len());
        for roi_node in &rois {
            let mut roi = Roi::default();

            let type_attr = roi_node.get_attribute("type");
            if !type_attr.trim().is_empty() {
                roi.roi_type = type_attr.trim().parse::<i32>().unwrap_or(0);
            }
            let color = roi_node.get_attribute("color");
            if !color.trim().is_empty() {
                roi.color = color.trim().parse::<i64>().unwrap_or(0);
            }
            roi.name = roi_node.get_attr("name");
            roi.font_name = roi_node.get_attr("fontName");
            roi.font_size = roi_node.get_attr("fontSize");

            if let Some(trans_x) = Self::data_parse_double(&roi_node.get_attribute("transTransX")) {
                if physical_size_x != 0.0 {
                    roi.trans_x = trans_x / physical_size_x;
                }
            }
            if let Some(trans_y) = Self::data_parse_double(&roi_node.get_attribute("transTransY")) {
                if physical_size_y != 0.0 {
                    roi.trans_y = trans_y / physical_size_y;
                }
            }
            if let Some(scale_x) = Self::data_parse_double(&roi_node.get_attribute("transScalingX"))
            {
                if physical_size_x != 0.0 {
                    roi.scale_x = scale_x / physical_size_x;
                }
            }
            if let Some(scale_y) = Self::data_parse_double(&roi_node.get_attribute("transScalingY"))
            {
                if physical_size_y != 0.0 {
                    roi.scale_y = scale_y / physical_size_y;
                }
            }
            if let Some(rotation) = Self::data_parse_double(&roi_node.get_attribute("transRotation"))
            {
                roi.rotation = rotation;
            }
            let linewidth = roi_node.get_attribute("linewidth");
            if !linewidth.trim().is_empty() {
                if let Ok(lw) = linewidth.trim().parse::<i32>() {
                    roi.linewidth = lw;
                }
            }
            roi.text = roi_node.get_attr("text");

            let vertices = get_nodes(roi_node, "Vertex");
            if vertices.is_empty() {
                continue;
            }
            for vertex in &vertices {
                let xx = vertex.get_attribute("x");
                let yy = vertex.get_attribute("y");
                if !xx.trim().is_empty() {
                    if let Some(x) = Self::data_parse_double(xx.trim()) {
                        roi.x.push(x);
                    }
                }
                if !yy.trim().is_empty() {
                    if let Some(y) = Self::data_parse_double(yy.trim()) {
                        roi.y.push(y);
                    }
                }
            }
            image_rois.push(roi);

            if has_roi_nodes {
                self.buffer.alternate_center = true;
            }
        }
        self.buffer.image_rois = image_rois;
    }

    /// Mirror of LMSMetadataExtractor.translateSingleROIs.
    fn translate_single_rois(&mut self, image_node: &XmlNode) {
        if !self.buffer.image_rois.is_empty() {
            return;
        }
        let roi_roots = get_nodes(image_node, "ROI");
        if roi_roots.is_empty() {
            return;
        }
        let children_list = get_nodes(roi_roots[0], "Children");
        if children_list.is_empty() {
            return;
        }
        let elements = get_nodes(children_list[0], "Element");
        if elements.is_empty() {
            return;
        }

        let physical_size_x = self.buffer.physical_size_x.unwrap_or(0.0);
        let physical_size_y = self.buffer.physical_size_y.unwrap_or(0.0);

        let mut image_rois: Vec<Roi> = Vec::with_capacity(elements.len());
        for element in &elements {
            let rois = get_nodes(element, "ROISingle");
            let Some(roi_node) = rois.first() else {
                continue;
            };
            let mut roi = Roi::default();

            let type_attr = roi_node.get_attribute("RoiType");
            if !type_attr.trim().is_empty() {
                roi.roi_type = type_attr.trim().parse::<i32>().unwrap_or(0);
            }
            let color = roi_node.get_attribute("Color");
            if !color.trim().is_empty() {
                roi.color = color.trim().parse::<i64>().unwrap_or(0);
            }
            // parent = roiNode.getParentNode().getParentNode(); roi.name = parent.Name.
            roi.name = Some(element.get_attribute("Name"));

            let vertices = get_nodes(roi_node, "P");
            for vertex in &vertices {
                let xx = vertex.get_attribute("X");
                let yy = vertex.get_attribute("Y");
                if !xx.trim().is_empty() {
                    if let Some(x) = Self::data_parse_double(xx.trim()) {
                        if physical_size_x != 0.0 {
                            roi.x.push(x / physical_size_x);
                        }
                    }
                }
                if !yy.trim().is_empty() {
                    if let Some(y) = Self::data_parse_double(yy.trim()) {
                        if physical_size_y != 0.0 {
                            roi.y.push(y / physical_size_y);
                        }
                    }
                }
            }

            if let Some(transform) = get_nodes(roi_node, "Transformation").first() {
                if let Some(rotation) =
                    Self::data_parse_double(&transform.get_attribute("Rotation"))
                {
                    roi.rotation = rotation;
                }
                if let Some(scaling) = get_nodes(transform, "Scaling").first() {
                    if let Some(scale_x) =
                        Self::data_parse_double(&scaling.get_attribute("XScale"))
                    {
                        roi.scale_x = scale_x;
                    }
                    if let Some(scale_y) =
                        Self::data_parse_double(&scaling.get_attribute("YScale"))
                    {
                        roi.scale_y = scale_y;
                    }
                }
                if let Some(translation) = get_nodes(transform, "Translation").first() {
                    if let Some(trans_x) =
                        Self::data_parse_double(&translation.get_attribute("X"))
                    {
                        if physical_size_x != 0.0 {
                            roi.trans_x = trans_x / physical_size_x;
                        }
                    }
                    if let Some(trans_y) =
                        Self::data_parse_double(&translation.get_attribute("Y"))
                    {
                        if physical_size_y != 0.0 {
                            roi.trans_y = trans_y / physical_size_y;
                        }
                    }
                }
            }
            image_rois.push(roi);
        }
        self.buffer.image_rois = image_rois;
    }

    /// Mirror of LMSMetadataExtractor.translateDetectors.
    fn translate_detectors(&mut self, image_node: &XmlNode) {
        let definitions = get_nodes_with_parents(image_node, "ATLConfocalSettingDefinition");
        if definitions.is_empty() {
            return;
        }
        let effective_c = self.get_effective_size_c();
        let mut channels: Vec<String> = Vec::new();
        let mut next_channel = 0i32;

        for (definition_node, parent_name) in &definitions {
            let is_master = parent_name.ends_with("Master");
            let detectors = get_nodes(definition_node, "Detector");
            if detectors.is_empty() {
                return;
            }
            let mut count = 0usize;
            for detector in &detectors {
                let multibands: Vec<&XmlNode> = if !is_master {
                    get_nodes(definition_node, "MultiBand")
                } else {
                    Vec::new()
                };

                let v = detector.get_attribute("Gain");
                let gain = if v.trim().is_empty() {
                    None
                } else {
                    Self::data_parse_double(v.trim())
                };
                let v = detector.get_attribute("Offset");
                let offset = if v.trim().is_empty() {
                    None
                } else {
                    Self::data_parse_double(v.trim())
                };

                let active = detector.get_attribute("IsActive") == "1";
                let c = detector.get_attribute("Channel");
                let channel = if c.trim().is_empty() {
                    0
                } else {
                    c.parse::<i32>().unwrap_or(0)
                };

                if active {
                    if let Some(name) = self.buffer.detector_indexes.get(&channel) {
                        self.buffer.detector_models.push(name.clone());
                    }

                    let mut multiband: Option<&XmlNode> = None;
                    for mb in &multibands {
                        if channel == mb.get_attribute("Channel").parse::<i32>().unwrap_or(0) {
                            multiband = Some(mb);
                            break;
                        }
                    }

                    if let Some(mb) = multiband {
                        let dye = mb.get_attribute("DyeName");
                        if !channels.contains(&dye) {
                            channels.push(dye);
                        }
                        let cut_in = Self::data_parse_double(&mb.get_attribute("LeftWorld"));
                        let cut_out = Self::data_parse_double(&mb.get_attribute("RightWorld"));
                        if let Some(cut_in) = cut_in {
                            if cut_in as i32 > 0 {
                                self.buffer.cut_ins.push(cut_in.round());
                            }
                        }
                        if let Some(cut_out) = cut_out {
                            if cut_out as i32 > 0 {
                                self.buffer.cut_outs.push(cut_out.round());
                            }
                        }
                    } else {
                        channels.push(String::new());
                    }

                    if !is_master {
                        if channel < next_channel {
                            next_channel = 0;
                        }
                        if next_channel < effective_c {
                            let nc = next_channel as usize;
                            if nc < self.buffer.gains.len() {
                                self.buffer.gains[nc] = gain;
                            }
                            if nc < self.buffer.detector_offsets.len() {
                                self.buffer.detector_offsets[nc] = offset;
                            }
                        }
                        next_channel += 1;
                    }
                } else {
                    count += 1;
                }
                if active {
                    self.buffer.active_detector.push(active);
                }
            }
            if !is_master {
                self.buffer.laser_active.push(count < detectors.len());
            }
        }

        // Backfill channel names from detector dye names.
        if !self.buffer.channel_names.is_empty() {
            for i in 0..effective_c {
                let index = i + channels.len() as i32 - effective_c;
                if index >= 0 && (index as usize) < channels.len() {
                    let i = i as usize;
                    if self.buffer.channel_names[i]
                        .as_deref()
                        .map(|n| n.trim().is_empty())
                        .unwrap_or(true)
                    {
                        self.buffer.channel_names[i] = Some(channels[index as usize].clone());
                    }
                }
            }
        }
    }
}

impl Default for LmsMetadataExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// Mirror of FormatTools.pixelTypeFromBytes(bytes, signed=false, allowLong=true).
fn pixel_type_from_bytes(bytes: i32) -> Result<PixelType> {
    match bytes {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        4 => Ok(PixelType::Uint32),
        8 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "Leica LMS: unsupported X bytesInc {bytes}"
        ))),
    }
}

fn pack_color(r: i32, g: i32, b: i32, a: i32) -> i32 {
    ((r & 0xff) << 24) | ((g & 0xff) << 16) | ((b & 0xff) << 8) | (a & 0xff)
}

// ---------------------------------------------------------------------------
// Public entry point used by XlefReader
// ---------------------------------------------------------------------------

/// Build an `ImageMetadata` from an XLIF image node by faithfully running the
/// LMS dimension/channel extraction. Mirrors the per-image portion of
/// LMSFileReader.translateMetadata / LMSMetadataExtractor.translateImage that
/// populates CoreMetadata and the channel/physical-size buffer.
pub fn image_metadata_from_xlif(xlif: &XlifDocument) -> Result<ImageMetadata> {
    let image_node = xlif.get_image_node().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Leica XLIF {} has no Image node",
            xlif.filepath.display()
        ))
    })?;

    let is_tif_or_jpeg =
        matches!(xlif.image_format, ImageFormat::Tif | ImageFormat::Jpeg);

    let mut extractor = LmsMetadataExtractor::new();
    extractor.image_format = xlif.image_format;
    extractor.translate_channel_descriptions(image_node);
    // Java reads old-physical-size from metadata options; default is false.
    let core = extractor.translate_dimension_descriptions(image_node, false, is_tif_or_jpeg)?;
    // Hardware / ROI translators (run after dimensions, like translateImage).
    extractor.translate_image_extras(image_node);

    let mut meta = ImageMetadata {
        size_x: core.size_x,
        size_y: core.size_y,
        size_z: core.size_z,
        size_c: core.size_c,
        size_t: core.size_t,
        pixel_type: core.pixel_type,
        bits_per_pixel: (core.pixel_type.bytes_per_sample() as u8) * 8,
        image_count: core.image_count.max(1),
        dimension_order: parse_dimension_order(&core.dimension_order),
        is_rgb: core.rgb,
        is_interleaved: core.interleaved,
        is_indexed: core.indexed,
        is_little_endian: true,
        ..Default::default()
    };

    // Image name and per-channel / physical-size metadata.
    if let Some(name) = xlif.get_image_name() {
        meta.series_metadata.insert(
            "xlef.lms.image.name".into(),
            MetadataValue::String(name),
        );
    }
    if let Some(px) = extractor.buffer.physical_size_x {
        if px.is_finite() && px != 0.0 {
            meta.series_metadata.insert(
                "xlef.lms.physical_size_x".into(),
                MetadataValue::Float(px.abs()),
            );
        }
    }
    if let Some(py) = extractor.buffer.physical_size_y {
        if py.is_finite() && py != 0.0 {
            meta.series_metadata.insert(
                "xlef.lms.physical_size_y".into(),
                MetadataValue::Float(py.abs()),
            );
        }
    }
    if let Some(pz) = extractor.buffer.z_step {
        if pz.is_finite() && pz != 0.0 {
            meta.series_metadata.insert(
                "xlef.lms.physical_size_z".into(),
                MetadataValue::Float(pz.abs()),
            );
        }
    }
    meta.series_metadata.insert(
        "xlef.lms.tile_count".into(),
        MetadataValue::Int(extractor.buffer.tile_count as i64),
    );
    for (index, channel) in extractor.buffer.channels.iter().enumerate() {
        let prefix = format!("xlef.lms.channel.{index}");
        if !channel.lut_name.is_empty() {
            meta.series_metadata.insert(
                format!("{prefix}.lut_name"),
                MetadataValue::String(channel.lut_name.clone()),
            );
        }
        if let Some(color) = extractor.buffer.channel_colors.get(index) {
            meta.series_metadata
                .insert(format!("{prefix}.ome_color"), MetadataValue::Int(*color as i64));
        }
    }

    emit_lms_hardware_metadata(&extractor, &mut meta);
    emit_lms_channel_lasers(&extractor, &mut meta);
    emit_lms_channel_detectors(&extractor, &mut meta);
    emit_lms_planes(&extractor, &mut meta);
    emit_lms_roi_metadata(&mut extractor, core.size_x as i32, core.size_y as i32, &mut meta);

    Ok(meta)
}

/// Project the LMS hardware buffer (microscope / objective / detector / laser /
/// filter / per-channel) into the `xlef.lms.*` alias keys consumed by XlefReader's
/// `xlef_lms_ome_metadata`. Mirrors the single-instrument projection that
/// MetadataStoreInitializer performs (index 0 = the active hardware).
fn emit_lms_hardware_metadata(extractor: &LmsMetadataExtractor, meta: &mut ImageMetadata) {
    let buffer = &extractor.buffer;
    let put_str = |meta: &mut ImageMetadata, key: &str, value: Option<&str>| {
        if let Some(value) = value {
            if !value.trim().is_empty() {
                meta.series_metadata
                    .insert(key.to_string(), MetadataValue::String(value.trim().to_string()));
            }
        }
    };
    let put_f64 = |meta: &mut ImageMetadata, key: &str, value: Option<f64>| {
        if let Some(value) = value {
            if value.is_finite() {
                meta.series_metadata
                    .insert(key.to_string(), MetadataValue::Float(value));
            }
        }
    };

    if let Some(desc) = &buffer.description {
        put_str(meta, "xlef.lms.description", Some(desc));
    }
    put_str(
        meta,
        "xlef.lms.microscope.0.name",
        buffer.microscope_model.as_deref(),
    );

    // Objective (MetadataStoreInitializer.initStandDetails).
    if buffer.objective_model.is_some()
        || buffer.magnification.is_some()
        || buffer.lens_na.is_some()
    {
        put_str(
            meta,
            "xlef.lms.objective.0.name",
            buffer.objective_model.as_deref(),
        );
        put_f64(
            meta,
            "xlef.lms.objective.0.magnification",
            buffer.magnification,
        );
        put_f64(
            meta,
            "xlef.lms.objective.0.numerical_aperture",
            buffer.lens_na,
        );
        // Java only sets immersion/correction when not the "Other" placeholder
        // produces a recognised enum; we forward the raw token.
        if let Some(imm) = &buffer.immersion {
            if imm != "Other" {
                put_str(meta, "xlef.lms.objective.0.immersion", Some(imm));
            }
        }
        if let Some(corr) = &buffer.correction {
            if corr != "Other" {
                put_str(meta, "xlef.lms.objective.0.correction", Some(corr));
            }
        }
    }
    put_str(
        meta,
        "xlef.lms.objective.0.serial_number",
        buffer.serial_number.as_deref(),
    );

    // Detector (MetadataStoreInitializer.initDetectorModels). The simplified
    // consumer model carries a single detector; emit the last (active) one with
    // its zoom-derived type and the first channel's gain/offset.
    if let Some(model) = buffer.detector_models.last() {
        put_str(meta, "xlef.lms.detector.0.name", Some(model));
        meta.series_metadata.insert(
            "xlef.lms.detector.0.type".into(),
            MetadataValue::String("PMT".into()),
        );
        put_f64(
            meta,
            "xlef.lms.detector.0.gain",
            buffer.gains.iter().flatten().next().copied(),
        );
        put_f64(
            meta,
            "xlef.lms.detector.0.offset",
            buffer.detector_offsets.iter().flatten().next().copied(),
        );
    }

    // Laser (MetadataStoreInitializer.initLasers): first non-zero wavelength.
    if let Some(&wavelength) = buffer.laser_wavelength.iter().find(|&&w| w != 0.0) {
        put_str(meta, "xlef.lms.laser.0.name", Some("Laser"));
        put_f64(meta, "xlef.lms.laser.0.wavelength", Some(wavelength));
    }

    // Filter (MetadataStoreInitializer.initFilterModels): first cut-in/out pair.
    if !buffer.filter_models.is_empty() || !buffer.cut_ins.is_empty() {
        put_str(
            meta,
            "xlef.lms.filter.0.name",
            buffer.filter_models.first().map(|s| s.as_str()),
        );
        put_f64(meta, "xlef.lms.filter.0.cut_in", buffer.cut_ins.first().copied());
        put_f64(
            meta,
            "xlef.lms.filter.0.cut_out",
            buffer.cut_outs.first().copied(),
        );
    }

    // Per-channel names + excitation wavelengths (initDetectorModels / scanner).
    for (index, name) in buffer.channel_names.iter().enumerate() {
        if let Some(name) = name {
            put_str(
                meta,
                &format!("xlef.lms.channel.{index}.name"),
                Some(name),
            );
        }
    }
    for (index, ex) in buffer.ex_waves.iter().enumerate() {
        if let Some(ex) = ex {
            if *ex > 1.0 {
                put_f64(
                    meta,
                    &format!("xlef.lms.channel.{index}.excitation_wavelength"),
                    Some(*ex),
                );
            }
        }
    }
}

/// Mirror of FormatReader.getZCTCoords (FormatTools.getZCTCoords): map a plane
/// index to its (z, c, t) coordinate given the core dimension order and sizes.
fn lms_get_zct_coords(extractor: &LmsMetadataExtractor, index: i32) -> [i32; 3] {
    let size_z = extractor.core_size_z.max(1) as i32;
    let size_c = extractor.get_effective_size_c().max(1);
    let size_t = extractor.core_size_t.max(1) as i32;
    let order: Vec<char> = extractor.core_dimension_order.chars().collect();
    // Position of Z/C/T within the dimension order string (after X,Y).
    let pos = |dim: char| -> usize {
        order
            .iter()
            .position(|&c| c == dim)
            .map(|p| p.saturating_sub(2))
            .unwrap_or(0)
    };
    let (iz, ic, it) = (pos('Z'), pos('C'), pos('T'));
    // Build the radix per axis according to its position in the order.
    let mut order_len = [0usize; 3];
    order_len[iz] = size_z as usize;
    order_len[ic] = size_c as usize;
    order_len[it] = size_t as usize;
    // FormatTools.rasterToPosition over the three middle dimensions.
    let mut remaining = index;
    let mut positions = [0i32; 3];
    for p in 0..3 {
        let dim = order_len[p].max(1) as i32;
        positions[p] = remaining % dim;
        remaining /= dim;
    }
    [positions[iz], positions[ic], positions[it]]
}

/// Mirror of MetadataStoreInitializer.initLasers — the per-channel light-source
/// assignment loop (laser-intensity validity filtering, the nextChannel walking,
/// attenuation, excitation wavelength). Emits xlef.lms.channel.N.* alias keys.
///
/// NOTE: the Java `setLightPathEmissionFilterRef` inside this method (the filter
/// reference following the nextFilter walk) is commented out, so we faithfully
/// emit no corresponding metadata for it here; the filter light-path reference is
/// produced only by initDetectorModels (where the call is live).
fn emit_lms_channel_lasers(extractor: &LmsMetadataExtractor, meta: &mut ImageMetadata) {
    let buffer = &extractor.buffer;
    // lasers / laserIntensities / active / frap (single image, index 0 in Java).
    let mut lasers: Vec<f64> = buffer.laser_wavelength.clone();
    let laser_intensities: &Vec<f64> = &buffer.laser_intensity;
    let active: &Vec<bool> = &buffer.laser_active;
    let frap: &Vec<bool> = &buffer.laser_frap;
    if lasers.is_empty() {
        return;
    }

    // Remove zero-wavelength lasers in place.
    let mut laser_index = 0usize;
    while laser_index < lasers.len() {
        if lasers[laser_index] == 0.0 {
            lasers.remove(laser_index);
        } else {
            laser_index += 1;
        }
    }

    // The LightSource ids/wavelengths are emitted by emit_lms_hardware_metadata's
    // instrument projection; here we only mirror the per-channel assignment.

    let mut ignored_channels: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut valid_intensities: Vec<i32> = Vec::new();
    let size = lasers.len() as i32;
    let mut channels: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for laser in 0..laser_intensities.len() {
        let intensity = laser_intensities[laser];
        let channel = if size > 0 { laser as i32 / size } else { 0 };
        if intensity < 100.0 {
            valid_intensities.push(laser as i32);
            channels.insert(channel);
        }
        ignored_channels.insert(channel);
    }
    // Remove channels w/o valid intensities.
    for c in &channels {
        ignored_channels.remove(c);
    }
    // Remove entries if channel has 2 wavelengths (e.g. 30% 458 70% 633).
    let s = valid_intensities.len() as i32;
    let mut to_remove: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let as_len = active.len() as i32;
    for j in 0..s {
        if j < as_len && !active[j as usize] {
            to_remove.insert(valid_intensities[j as usize]);
        }
        let jj = j + 1;
        if jj < s && size > 0 {
            let v = valid_intensities[j as usize] / size;
            let vv = valid_intensities[jj as usize] / size;
            if vv == v {
                to_remove.insert(valid_intensities[j as usize]);
                to_remove.insert(valid_intensities[jj as usize]);
                ignored_channels.insert(j);
            }
        }
    }
    if !to_remove.is_empty() {
        valid_intensities.retain(|v| !to_remove.contains(v));
    }

    // noNames detection (channelNames[index]).
    let mut no_names = true;
    for name in &buffer.channel_names {
        if let Some(name) = name {
            if !name.is_empty() {
                no_names = false;
                break;
            }
        }
    }
    if !no_names && !frap.is_empty() {
        for k in 0..frap.len() {
            if !frap[k] {
                no_names = true;
                break;
            }
        }
    }

    let effective_c = extractor.get_effective_size_c();
    // nextFilter exists in Java but is only consumed by the commented-out
    // setLightPathEmissionFilterRef, so it produces no metadata; we still mirror
    // the nextFilter advancement to match the loop's control flow side effects.
    let mut next_filter = 0i32;
    let mut next_channel = 0i32;
    for k in 0..valid_intensities.len() {
        let laser_array_index = valid_intensities[k];
        let intensity = laser_intensities[laser_array_index as usize];
        let laser = if !lasers.is_empty() {
            laser_array_index % lasers.len() as i32
        } else {
            0
        };
        let wavelength = lasers[laser as usize];
        if wavelength != 0.0 {
            while ignored_channels.contains(&next_channel) {
                next_channel += 1;
            }
            while next_channel < effective_c
                && (next_channel as usize) < buffer.channel_names.len()
                && (buffer.channel_names[next_channel as usize]
                    .as_deref()
                    .map(|n| n.is_empty())
                    .unwrap_or(true)
                    && !no_names)
            {
                next_channel += 1;
            }
            if next_channel < effective_c {
                let prefix = format!("xlef.lms.channel.{next_channel}");
                // store.setChannelLightSourceSettingsID(LightSource:series:laser)
                meta.series_metadata.insert(
                    format!("{prefix}.light_source_settings_id"),
                    MetadataValue::String(format!("LightSource:0:{laser}")),
                );
                // PercentFraction(intensity / 100f) → 0..1 fraction.
                meta.series_metadata.insert(
                    format!("{prefix}.light_source_settings_attenuation"),
                    MetadataValue::Float((intensity as f32 / 100.0_f32) as f64),
                );
                // FormatTools.getExcitationWavelength(wavelength).
                if wavelength > 0.0 {
                    meta.series_metadata.insert(
                        format!("{prefix}.excitation_wavelength"),
                        MetadataValue::Float(wavelength),
                    );
                }

                if wavelength > 0.0 {
                    if next_filter >= buffer.cut_ins.len() as i32 {
                        next_channel += 1;
                        continue;
                    }
                    let mut cut_in = buffer.cut_ins[next_filter as usize];
                    while cut_in - wavelength > 20.0 {
                        next_filter += 1;
                        if next_filter < buffer.cut_ins.len() as i32 {
                            cut_in = buffer.cut_ins[next_filter as usize];
                        } else {
                            break;
                        }
                    }
                    if next_filter < buffer.cut_ins.len() as i32 {
                        // store.setLightPathEmissionFilterRef(...) is commented out
                        // in Java — no metadata emitted; only advance nextFilter.
                        next_filter += 1;
                    }
                }
            }
        }
        next_channel += 1;
    }
}

/// Mirror of MetadataStoreInitializer.initDetectorModels — the per-channel
/// detector-settings assignment loop plus channel name / pinhole / excitation /
/// colour / emission-filter light-path reference. Emits xlef.lms.channel.N.*
/// alias keys.
fn emit_lms_channel_detectors(extractor: &LmsMetadataExtractor, meta: &mut ImageMetadata) {
    let buffer = &extractor.buffer;
    let effective_c = extractor.get_effective_size_c();
    let size_c = extractor.core_size_c.max(1) as i32;

    let detectors: &Vec<String> = &buffer.detector_models;
    let detectors_present = !detectors.is_empty();

    let active_detectors: &Vec<bool> = &buffer.active_detector;
    let active_present = !active_detectors.is_empty();
    let first_detector = if active_present {
        active_detectors.len() as i32 - effective_c
    } else {
        0
    };
    let mut next_detector = first_detector;

    let mut next_filter = 0i32;
    let mut next_filter_detector = 0i32;

    // Special case: trailing active detectors imply a filter-detector offset.
    if active_present
        && active_detectors.len() as i32 > buffer.cut_ins.len() as i32
        && *active_detectors.last().unwrap()
        && active_detectors.len() >= 2
        && active_detectors[active_detectors.len() - 2]
    {
        next_filter_detector = active_detectors.len() as i32 - buffer.cut_ins.len() as i32;
        if buffer.cut_ins.len() as i32 > buffer.filter_models.len() as i32 {
            next_filter_detector += buffer.filter_models.len() as i32;
            next_filter += buffer.filter_models.len() as i32;
        }
    }

    for c in 0..effective_c {
        let prefix = format!("xlef.lms.channel.{c}");
        if active_present {
            while next_detector >= 0
                && next_detector < active_detectors.len() as i32
                && !active_detectors[next_detector as usize]
            {
                next_detector += 1;
            }
            if next_detector < active_detectors.len() as i32
                && detectors_present
                && next_detector - first_detector < detectors.len() as i32
            {
                // store.setDetectorSettingsID(Detector:series:(nextDetector-first)).
                meta.series_metadata.insert(
                    format!("{prefix}.detector_ref"),
                    MetadataValue::String(format!(
                        "Detector:0:{}",
                        next_detector - first_detector
                    )),
                );
                next_detector += 1;

                if (c as usize) < buffer.detector_offsets.len() {
                    if let Some(off) = buffer.detector_offsets[c as usize] {
                        meta.series_metadata.insert(
                            format!("{prefix}.detector_settings_offset"),
                            MetadataValue::Float(off),
                        );
                    }
                }
                if (c as usize) < buffer.gains.len() {
                    if let Some(gain) = buffer.gains[c as usize] {
                        meta.series_metadata.insert(
                            format!("{prefix}.detector_settings_gain"),
                            MetadataValue::Float(gain),
                        );
                    }
                }
            }
        }

        // Channel name (emitted by hardware metadata already; harmless to keep
        // there — initDetectorModels sets it via setChannelName). We leave names
        // to emit_lms_hardware_metadata to avoid duplication.

        // Pinhole size (UNITS.MICROMETER).
        if let Some(pinhole) = buffer.pinhole {
            meta.series_metadata.insert(
                format!("{prefix}.pinhole_size"),
                MetadataValue::Float(pinhole),
            );
        }

        // Excitation wavelength from exWaves (>1).
        if (c as usize) < buffer.ex_waves.len() {
            if let Some(ex) = buffer.ex_waves[c as usize] {
                if ex > 1.0 {
                    meta.series_metadata.insert(
                        format!("{prefix}.excitation_wavelength"),
                        MetadataValue::Float(ex),
                    );
                }
            }
        }

        // Channel colour (only when not RGB) — already projected via ome_color in
        // the channel loop, so no separate key is needed here.
        let channel_color = buffer
            .channel_colors
            .get(c as usize)
            .copied()
            .unwrap_or(-1);

        // Emission-filter light path (this setLightPathEmissionFilterRef IS live
        // in Java, unlike the one in initLasers). Record the referenced filter id
        // per channel under xlef.lms.channel.N.emission_filter_ref.
        if channel_color != -1 && next_filter >= 0 {
            if next_detector - first_detector != size_c
                && next_detector >= buffer.cut_ins.len() as i32
            {
                while next_filter_detector < first_detector {
                    // store.setFilterID(Filter:series:nextFilter) — instrument-level
                    // filter ids are produced by the instrument projection; advance.
                    next_filter_detector += 1;
                    next_filter += 1;
                }
            }
            while active_present
                && next_filter_detector < active_detectors.len() as i32
                && !active_detectors[next_filter_detector as usize]
            {
                next_filter_detector += 1;
                next_filter += 1;
            }
            // store.setLightPathEmissionFilterRef(Filter:series:nextFilter, ...).
            meta.series_metadata.insert(
                format!("{prefix}.emission_filter_ref"),
                MetadataValue::String(format!("Filter:0:{next_filter}")),
            );
            next_filter_detector += 1;
            next_filter += 1;
        }
    }
}

/// Mirror of MetadataStoreInitializer.initImageDetails' plane loop — plane
/// positions, deltaT (from timestamps), and exposure times via getZCTCoords.
/// Emits xlef.lms.plane.N.* alias keys (one per image plane).
fn emit_lms_planes(extractor: &LmsMetadataExtractor, meta: &mut ImageMetadata) {
    let buffer = &extractor.buffer;
    let image_count = extractor.get_image_count();

    for image in 0..image_count {
        let prefix = format!("xlef.lms.plane.{image}");
        // posX/posY (field positions override; series 0 here), swap/flip handling.
        let mut x_pos = buffer.pos_x;
        let mut y_pos = buffer.pos_y;
        // fieldPos override only applies for series < fieldPos.size(); single image
        // (series 0) — use index 0 if present.
        if let Some(Some(fx)) = buffer.field_pos_x.first() {
            x_pos = Some(*fx);
        }
        if let Some(Some(fy)) = buffer.field_pos_y.first() {
            y_pos = Some(*fy);
        }
        if buffer.swap_xy {
            std::mem::swap(&mut x_pos, &mut y_pos);
        }
        if buffer.flip_x {
            x_pos = x_pos.map(|v| -v);
        }
        if buffer.flip_y {
            y_pos = y_pos.map(|v| -v);
        }

        if let Some(x) = x_pos {
            meta.series_metadata.insert(
                format!("{prefix}.position_x"),
                MetadataValue::Float(x),
            );
        }
        if let Some(y) = y_pos {
            meta.series_metadata.insert(
                format!("{prefix}.position_y"),
                MetadataValue::Float(y),
            );
        }
        if let Some(z) = buffer.pos_z {
            meta.series_metadata.insert(
                format!("{prefix}.position_z"),
                MetadataValue::Float(z),
            );
        }

        // deltaT from timestamps.
        if let Some(Some(mut timestamp)) = buffer.timestamps.get(image as usize).copied() {
            let first = buffer.timestamps.first().and_then(|t| *t);
            if first == Some(buffer.acquired_date) {
                timestamp -= buffer.acquired_date;
            } else if timestamp == buffer.acquired_date && image > 0 {
                if let Some(t0) = first {
                    timestamp = t0;
                }
            }
            meta.series_metadata.insert(
                format!("{prefix}.delta_t"),
                MetadataValue::Float(timestamp),
            );
        }

        // Exposure time via getZCTCoords()[1].
        if !buffer.exp_times.is_empty() {
            let c = lms_get_zct_coords(extractor, image)[1];
            if let Some(Some(exp)) = buffer.exp_times.get(c as usize).copied() {
                meta.series_metadata.insert(
                    format!("{prefix}.exposure_time"),
                    MetadataValue::Float(exp),
                );
            }
        }
    }
}

/// Project the LMS ROI buffer into `xlef.lms.roi.N.*` alias keys consumed by
/// XlefReader's `xlef_lms_ome_metadata`. Mirrors ROI.storeROI's shape selection.
fn emit_lms_roi_metadata(
    extractor: &mut LmsMetadataExtractor,
    size_x: i32,
    size_y: i32,
    meta: &mut ImageMetadata,
) {
    let alternate_center = extractor.buffer.alternate_center;
    // storeROI mutates the ROI (normalize); take ownership to satisfy the borrow.
    let mut rois = std::mem::take(&mut extractor.buffer.image_rois);
    for (index, roi) in rois.iter_mut().enumerate() {
        let prefix = format!("xlef.lms.roi.{index}");
        if let Some(name) = &roi.name {
            if !name.is_empty() {
                meta.series_metadata.insert(
                    format!("{prefix}.name"),
                    MetadataValue::String(name.clone()),
                );
            }
        }
        let shape = roi.store_roi(size_x, size_y, alternate_center);
        match shape {
            Some(StoredShape::Rectangle { x, y, width, height }) => {
                meta.series_metadata.insert(
                    format!("{prefix}.shape"),
                    MetadataValue::String("Rectangle".into()),
                );
                meta.series_metadata
                    .insert(format!("{prefix}.x"), MetadataValue::Float(x));
                meta.series_metadata
                    .insert(format!("{prefix}.y"), MetadataValue::Float(y));
                meta.series_metadata
                    .insert(format!("{prefix}.width"), MetadataValue::Float(width));
                meta.series_metadata
                    .insert(format!("{prefix}.height"), MetadataValue::Float(height));
            }
            Some(StoredShape::Line { x1, y1, x2, y2 }) => {
                meta.series_metadata.insert(
                    format!("{prefix}.shape"),
                    MetadataValue::String("Line".into()),
                );
                meta.series_metadata
                    .insert(format!("{prefix}.x1"), MetadataValue::Float(x1));
                meta.series_metadata
                    .insert(format!("{prefix}.y1"), MetadataValue::Float(y1));
                meta.series_metadata
                    .insert(format!("{prefix}.x2"), MetadataValue::Float(x2));
                meta.series_metadata
                    .insert(format!("{prefix}.y2"), MetadataValue::Float(y2));
            }
            Some(StoredShape::Polygon { points }) => {
                meta.series_metadata.insert(
                    format!("{prefix}.shape"),
                    MetadataValue::String("Polygon".into()),
                );
                let encoded = points
                    .iter()
                    .map(|(px, py)| format!("{px},{py}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                meta.series_metadata.insert(
                    format!("{prefix}.points"),
                    MetadataValue::String(encoded),
                );
            }
            None => {}
        }
    }
}

/// Convert an LMS dimension-order string (e.g. "XYCZT") to the enum, falling
/// back to XYCZT (the Bio-Formats default) for unrecognised orders.
fn parse_dimension_order(order: &str) -> DimensionOrder {
    match order {
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYTCZ" => DimensionOrder::XYTCZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        _ => DimensionOrder::XYCZT,
    }
}
