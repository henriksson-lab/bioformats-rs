# Indexed thumbnail destination oracle

`PaletteGrid.java` exercises the native indexed destination conversion used by Bio-Formats `AWTImageTools.makeBuffered(scaled, originalColorModel)`. It draws all 256 gray RGB values at every position in the 8x8 ordered-dither matrix into ten indexed palettes: ascending/reversed/permuted gray ramps, complete/incomplete RGB corner palettes, dense gray palettes with complete/incomplete corners, and a sparse gray palette with duplicate entries.

Reproduce with the local JDK (goldens recorded using OpenJDK 19.0.2):

```sh
javac -d /tmp/thumbnail-oracle tests/fixtures/thumbnail/PaletteGrid.java
java -Djava.awt.headless=true -cp /tmp/thumbnail-oracle PaletteGrid /tmp/thumbnail-oracle
```

The `.bin` files contain 2048×8 palette indices; `.pal` contains the ARGB palette as little-endian uint32. The Rust regression in `src/common/thumbnail_indexed.rs` checks FNV-1a-64 digests of all 16,384 resulting indices for each palette, retaining the complete output coverage without opaque binary fixtures.

The inverse-color cube was additionally compared cell-for-cell with the local libawt `initCubemap` export for random palettes of lengths 2, 3, 16 and 256 (zero differences across 32³ cells). That algorithm propagates seeds through six neighboring quantized cells in distance shells and has observable palette-order tie behavior; it is not Euclidean nearest-color selection. Ordered error constants match libawt `initDitherTables`/`std_img_oda_red`, `green` and `blue`. OpenJDK's `ByteIndexed.h` specifies clamping, 5-bit inverse-color-map conversion and the complete-primary palette optimization.
