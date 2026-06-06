/// BfParityOracle — reference oracle for Java↔Rust parity testing.
///
/// Given an image path, prints ONE line of JSON describing, per series:
///   - core metadata (sizeX/Y/Z/C/T, pixelType, bitsPerPixel, imageCount,
///     dimensionOrder, rgb/interleaved/indexed/littleEndian, rgbChannelCount,
///     resolutionCount)
///   - a bounded-region CRC32 of the first few planes (so gigapixel whole-slide
///     images don't allocate gigabytes — same region the Rust side reads)
/// and the OME metadata (per image: name, physical sizes, time increment,
/// per-channel name / samplesPerPixel / emission / excitation).
///
/// The JSON is hand-built (no JSON lib) so it only needs bioformats_package.jar.
///
/// Usage: java -cp bioformats_package.jar:<dir> BfParityOracle <path> [maxPlanes] [region]
///   maxPlanes default 8, region (square edge) default 256.

import loci.formats.ImageReader;
import loci.formats.FormatTools;
import loci.formats.meta.IMetadata;
import loci.formats.services.OMEXMLService;
import loci.common.services.ServiceFactory;
import loci.common.DebugTools;
import java.util.zip.CRC32;

public class BfParityOracle {
    public static void main(String[] args) {
        DebugTools.setRootLevel("ERROR");
        String path = args[0];
        int maxPlanes = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        int region = args.length > 2 ? Integer.parseInt(args[2]) : 256;

        ImageReader reader = new ImageReader();
        IMetadata ome = null;
        try {
            ServiceFactory factory = new ServiceFactory();
            OMEXMLService service = factory.getInstance(OMEXMLService.class);
            ome = service.createOMEXMLMetadata();
            reader.setMetadataStore(ome);
        } catch (Throwable t) {
            // OME store optional; continue with core metadata only.
            ome = null;
        }

        StringBuilder sb = new StringBuilder();
        try {
            reader.setId(path);
        } catch (Throwable t) {
            System.out.println("{\"ok\":false,\"error\":" + jstr(t.toString()) + "}");
            return;
        }

        sb.append("{\"ok\":true,\"seriesCount\":").append(reader.getSeriesCount()).append(",\"series\":[");
        for (int s = 0; s < reader.getSeriesCount(); s++) {
            reader.setSeries(s);
            if (s > 0) sb.append(",");
            sb.append("{\"index\":").append(s);
            sb.append(",\"sizeX\":").append(reader.getSizeX());
            sb.append(",\"sizeY\":").append(reader.getSizeY());
            sb.append(",\"sizeZ\":").append(reader.getSizeZ());
            sb.append(",\"sizeC\":").append(reader.getSizeC());
            sb.append(",\"sizeT\":").append(reader.getSizeT());
            sb.append(",\"pixelType\":").append(jstr(FormatTools.getPixelTypeString(reader.getPixelType())));
            sb.append(",\"bitsPerPixel\":").append(reader.getBitsPerPixel());
            sb.append(",\"imageCount\":").append(reader.getImageCount());
            sb.append(",\"dimensionOrder\":").append(jstr(reader.getDimensionOrder()));
            sb.append(",\"rgb\":").append(reader.isRGB());
            sb.append(",\"interleaved\":").append(reader.isInterleaved());
            sb.append(",\"indexed\":").append(reader.isIndexed());
            sb.append(",\"littleEndian\":").append(reader.isLittleEndian());
            sb.append(",\"rgbChannelCount\":").append(reader.getRGBChannelCount());
            sb.append(",\"resolutionCount\":").append(reader.getResolutionCount());

            int w = Math.min(reader.getSizeX(), region);
            int h = Math.min(reader.getSizeY(), region);
            int planes = Math.min(reader.getImageCount(), maxPlanes);
            sb.append(",\"planeCrc\":[");
            for (int p = 0; p < planes; p++) {
                if (p > 0) sb.append(",");
                String entry;
                try {
                    byte[] buf = reader.openBytes(p, 0, 0, w, h);
                    CRC32 crc = new CRC32();
                    crc.update(buf);
                    entry = "{\"plane\":" + p + ",\"w\":" + w + ",\"h\":" + h
                            + ",\"len\":" + buf.length + ",\"crc\":" + crc.getValue() + "}";
                } catch (Throwable t) {
                    entry = "{\"plane\":" + p + ",\"error\":" + jstr(t.toString()) + "}";
                }
                sb.append(entry);
            }
            sb.append("]}");
        }
        sb.append("],\"ome\":");
        sb.append(omeJson(ome));
        sb.append("}");
        System.out.println(sb);
        try { reader.close(); } catch (Throwable ignored) {}
    }

    private static String omeJson(IMetadata ome) {
        if (ome == null) return "null";
        StringBuilder sb = new StringBuilder();
        int imgCount;
        try { imgCount = ome.getImageCount(); } catch (Throwable t) { return "null"; }
        sb.append("{\"images\":[");
        for (int i = 0; i < imgCount; i++) {
            final int ii0 = i;
            if (i > 0) sb.append(",");
            sb.append("{");
            sb.append("\"name\":").append(jstr(safeImageName(ome, i)));
            sb.append(",\"physicalSizeX\":").append(lengthVal(() -> ome.getPixelsPhysicalSizeX(ii0) == null ? null : ome.getPixelsPhysicalSizeX(ii0).value().doubleValue()));
            sb.append(",\"physicalSizeY\":").append(lengthVal(() -> ome.getPixelsPhysicalSizeY(ii0) == null ? null : ome.getPixelsPhysicalSizeY(ii0).value().doubleValue()));
            sb.append(",\"physicalSizeZ\":").append(lengthVal(() -> ome.getPixelsPhysicalSizeZ(ii0) == null ? null : ome.getPixelsPhysicalSizeZ(ii0).value().doubleValue()));
            sb.append(",\"timeIncrement\":").append(lengthVal(() -> ome.getPixelsTimeIncrement(ii0) == null ? null : ome.getPixelsTimeIncrement(ii0).value().doubleValue()));
            sb.append(",\"channels\":[");
            int cc = 0;
            try { cc = ome.getChannelCount(i); } catch (Throwable ignored) {}
            for (int c = 0; c < cc; c++) {
                final int ci = c, ii = i;
                if (c > 0) sb.append(",");
                sb.append("{");
                sb.append("\"name\":").append(jstr(tryStr(() -> ome.getChannelName(ii, ci))));
                sb.append(",\"samplesPerPixel\":").append(intVal(() -> ome.getChannelSamplesPerPixel(ii, ci) == null ? null : ome.getChannelSamplesPerPixel(ii, ci).getValue()));
                sb.append(",\"emission\":").append(lengthVal(() -> ome.getChannelEmissionWavelength(ii, ci) == null ? null : ome.getChannelEmissionWavelength(ii, ci).value().doubleValue()));
                sb.append(",\"excitation\":").append(lengthVal(() -> ome.getChannelExcitationWavelength(ii, ci) == null ? null : ome.getChannelExcitationWavelength(ii, ci).value().doubleValue()));
                sb.append("}");
            }
            sb.append("]}");
        }
        sb.append("]}");
        return sb.toString();
    }

    private static String safeImageName(IMetadata ome, int i) {
        try { return ome.getImageName(i); } catch (Throwable t) { return null; }
    }

    interface StrSup { String get() throws Throwable; }
    interface DblSup { Double get() throws Throwable; }
    interface IntSup { Integer get() throws Throwable; }

    private static String tryStr(StrSup s) {
        try { return s.get(); } catch (Throwable t) { return null; }
    }
    private static String lengthVal(DblSup s) {
        try { Double v = s.get(); return v == null ? "null" : String.valueOf(v); }
        catch (Throwable t) { return "null"; }
    }
    private static String intVal(IntSup s) {
        try { Integer v = s.get(); return v == null ? "null" : String.valueOf(v); }
        catch (Throwable t) { return "null"; }
    }

    private static String jstr(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"':  b.append("\\\""); break;
                case '\\': b.append("\\\\"); break;
                case '\n': b.append("\\n");  break;
                case '\r': b.append("\\r");  break;
                case '\t': b.append("\\t");  break;
                default:
                    if (c < 0x20) b.append(String.format("\\u%04x", (int) c));
                    else b.append(c);
            }
        }
        return b.append("\"").toString();
    }
}
