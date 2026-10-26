// Synthetic oracle for common::thumbnail; run generate.py from the repository root.
import java.nio.*;
import java.nio.file.*;
import loci.formats.*;

public class ThumbnailOracle extends FormatReader {
  byte[] pixels;
  ThumbnailOracle(int w, int h, int type, int channels, boolean interleaved, byte[] data) {
    super("Thumbnail oracle", "raw");
    currentId = "synthetic.raw";
    core = new java.util.ArrayList<>();
    CoreMetadata m = new CoreMetadata(); core.add(m);
    m.sizeX=w; m.sizeY=h; m.sizeZ=1; m.sizeT=1; m.sizeC=channels;
    m.pixelType=type; m.imageCount=1; m.rgb=channels>1;
    m.interleaved=interleaved; m.littleEndian=true; m.dimensionOrder="XYCZT";
    pixels=data;
  }
  public byte[] openBytes(int n, byte[] buf, int x, int y, int w, int h) {
    System.arraycopy(pixels,0,buf,0,buf.length); return buf;
  }
  public static void main(String[] args) throws Exception {
    if (args.length == 1 && args[0].equals("--gray-table")) {
      java.awt.image.BufferedImage rgb = new java.awt.image.BufferedImage(256,1,java.awt.image.BufferedImage.TYPE_INT_RGB);
      for(int x=0;x<256;x++) rgb.setRGB(x,0,0xff000000 | x*0x010101);
      for(int type : new int[]{java.awt.image.DataBuffer.TYPE_FLOAT,java.awt.image.DataBuffer.TYPE_DOUBLE}) {
        java.awt.image.BufferedImage gray=loci.formats.gui.AWTImageTools.makeBuffered(rgb,loci.formats.gui.AWTImageTools.makeColorModel(1,type));
        byte[] bytes=loci.formats.gui.AWTImageTools.getPixelBytes(gray,true)[0];
        java.nio.ByteBuffer buffer=java.nio.ByteBuffer.wrap(bytes).order(java.nio.ByteOrder.LITTLE_ENDIAN);
        for(int x=0;x<256;x++) System.out.printf(type==4?"0x%08x,":"0x%016x,",type==4?buffer.getInt()&0xffffffffL:buffer.getLong());
        System.out.println();
      }
      return;
    }
    String name=args[0]; int w=Integer.parseInt(args[1]), h=Integer.parseInt(args[2]);
    int type=FormatTools.pixelTypeFromString(args[3]), c=Integer.parseInt(args[4]);
    byte[] bytes=Files.readAllBytes(Paths.get(args[5]));
    ThumbnailOracle r=new ThumbnailOracle(w,h,type,c,Boolean.parseBoolean(args[6]),bytes);
    byte[] thumb=r.openThumbBytes(0);
    Files.write(Paths.get(name),thumb);
    System.out.println(name+" "+r.getThumbSizeX()+"x"+r.getThumbSizeY()+" bytes="+thumb.length);
  }
}
