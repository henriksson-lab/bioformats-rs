/// Java Bio-Formats subset-read timing harness for bench/compare_subset.sh.
///
/// Usage: java -cp <bioformats_package.jar:classdir> BfSubsetBench
///        <path> <warmup_rounds> <measure_rounds> <planes_per_series>
///        <max_region_width> <max_region_height>
///
/// Prints key=value lines. Timing excludes JVM startup and includes
/// ImageReader.setId + centered region reads for each measured iteration.

import loci.common.DebugTools;
import loci.formats.IFormatReader;
import loci.formats.ImageReader;

public class BfSubsetBench {
    private static final class ReadStats {
        long bytes = 0;
        int series = 0;
        int planes = 0;
        int maxWidth = 0;
        int maxHeight = 0;
    }

    private static ReadStats readSubset(
        String path,
        int planesPerSeries,
        int maxRegionWidth,
        int maxRegionHeight
    ) throws Exception {
        IFormatReader reader = new ImageReader();
        ReadStats stats = new ReadStats();
        reader.setId(path);
        int seriesCount = reader.getSeriesCount();

        for (int series = 0; series < seriesCount; series++) {
            reader.setSeries(series);
            int sizeX = reader.getSizeX();
            int sizeY = reader.getSizeY();
            int imageCount = reader.getImageCount();
            int w = Math.max(1, Math.min(sizeX, maxRegionWidth));
            int h = Math.max(1, Math.min(sizeY, maxRegionHeight));
            int x = (sizeX - w) / 2;
            int y = (sizeY - h) / 2;
            int planes = Math.min(imageCount, planesPerSeries);

            stats.series++;
            stats.maxWidth = Math.max(stats.maxWidth, sizeX);
            stats.maxHeight = Math.max(stats.maxHeight, sizeY);

            for (int plane = 0; plane < planes; plane++) {
                byte[] bytes = reader.openBytes(plane, x, y, w, h);
                stats.bytes += bytes.length;
                stats.planes++;
            }
        }

        reader.close();
        return stats;
    }

    private static void printError(Throwable t) {
        System.out.println("status=error");
        System.out.println("error=" + oneLine(t.toString()));
    }

    private static String oneLine(String s) {
        return s.replace('\n', ' ').replace('\r', ' ');
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 6) {
            System.err.println(
                "usage: BfSubsetBench <path> <warmup> <measure> <planes_per_series> <max_w> <max_h>"
            );
            System.exit(2);
        }

        DebugTools.setRootLevel("ERROR");
        String path = args[0];
        int warmup = Integer.parseInt(args[1]);
        int measure = Integer.parseInt(args[2]);
        int planesPerSeries = Integer.parseInt(args[3]);
        int maxRegionWidth = Integer.parseInt(args[4]);
        int maxRegionHeight = Integer.parseInt(args[5]);

        if (measure <= 0 || planesPerSeries <= 0 || maxRegionWidth <= 0 || maxRegionHeight <= 0) {
            printError(new IllegalArgumentException(
                "measure, planes_per_series, max_w, and max_h must be positive"
            ));
            System.exit(1);
        }

        for (int i = 0; i < warmup; i++) {
            try {
                readSubset(path, planesPerSeries, maxRegionWidth, maxRegionHeight);
            }
            catch (Throwable t) {
                printError(t);
                System.exit(1);
            }
        }

        long totalNs = 0;
        ReadStats lastStats = new ReadStats();
        for (int i = 0; i < measure; i++) {
            long t0 = System.nanoTime();
            try {
                lastStats = readSubset(path, planesPerSeries, maxRegionWidth, maxRegionHeight);
            }
            catch (Throwable t) {
                printError(t);
                System.exit(1);
            }
            totalNs += System.nanoTime() - t0;
        }

        System.out.println("status=ok");
        System.out.println("avg_ns=" + (totalNs / measure));
        System.out.println("bytes=" + lastStats.bytes);
        System.out.println("series=" + lastStats.series);
        System.out.println("planes=" + lastStats.planes);
        System.out.println("max_width=" + lastStats.maxWidth);
        System.out.println("max_height=" + lastStats.maxHeight);
    }
}
