import loci.formats.IFormatReader;
import loci.formats.ImageReader;
import loci.common.DebugTools;

public class BioformatsReadBench {
  public static void main(String[] args) throws Exception {
    if (args.length < 1 || args.length > 2) {
      System.err.println("usage: BioformatsReadBench <path> [planes]");
      System.exit(2);
    }

    String path = args[0];
    int requestedPlanes = args.length == 2 ? Integer.parseInt(args[1]) : -1;
    DebugTools.enableLogging("OFF");
    IFormatReader reader = new ImageReader();

    long openStart = System.nanoTime();
    reader.setId(path);
    long openEnd = System.nanoTime();

    int imageCount = reader.getImageCount();
    int planes = requestedPlanes < 0 ? imageCount : Math.min(requestedPlanes, imageCount);
    long bytes = 0;
    long readStart = System.nanoTime();
    for (int i = 0; i < planes; i++) {
      bytes += reader.openBytes(i).length;
    }
    long readEnd = System.nanoTime();

    int sizeX = reader.getSizeX();
    int sizeY = reader.getSizeY();
    reader.close();

    double openMs = (openEnd - openStart) / 1_000_000.0;
    double readMs = (readEnd - readStart) / 1_000_000.0;
    System.out.printf(
      "file=%s size=%dx%d planes_read=%d image_count=%d bytes=%d open_ms=%.3f read_ms=%.3f total_ms=%.3f%n",
      path, sizeX, sizeY, planes, imageCount, bytes, openMs, readMs, openMs + readMs
    );
  }
}
