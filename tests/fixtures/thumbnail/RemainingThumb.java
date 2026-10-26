import loci.formats.*;
import loci.formats.in.FakeReader;
import loci.formats.gui.AWTImageTools;
import java.awt.image.BufferedImage;
import java.nio.file.*;
import java.util.*;
public class RemainingThumb {
 public static void main(String[] args) throws Exception {
  for(String type:new String[]{"uint8","uint16","int32","uint32"}) for(int rgb:new int[]{1,2,3}) for(int lut:new int[]{0,1,2,3,4}) {
   if(lut>0 && rgb!=1) continue;
   String id="case&sizeX=300&sizeY=200&sizeC="+rgb+"&rgb="+rgb+"&pixelType="+type+"&indexed="+(lut>0)+"&lutLength="+(lut>0?lut:3)+".fake";
   FakeReader r=new FakeReader();
   try {
    r.setId(id); byte[] raw=r.openBytes(0);
    BufferedImage img=AWTImageTools.openImage(raw,r,r.getSizeX(),r.getSizeY(),true);
    BufferedImage uns=AWTImageTools.makeUnsigned(img);
    byte[] thumb=r.openThumbBytes(0);
    String name=type+"-rgb"+rgb+"-lut"+lut;
    Files.write(Path.of(args[0],name+".raw"),raw); Files.write(Path.of(args[0],name+".thumb"),thumb);
    System.out.println("RESULT "+name+" OK raw="+raw.length+" thumb="+thumb.length+" geom="+r.getThumbSizeX()+"x"+r.getThumbSizeY()+" model="+img.getColorModel().getClass().getSimpleName()+" unsigned="+uns.getColorModel().getClass().getSimpleName()+" prefix="+HexFormat.of().formatHex(Arrays.copyOf(thumb,Math.min(40,thumb.length))));
   } catch(Throwable e) {System.out.println("RESULT "+type+"-rgb"+rgb+"-lut"+lut+" REJECT "+e); Throwable c=e.getCause(); while(c!=null){System.out.println("CAUSE "+c);c=c.getCause();}}
   finally {r.close();}
  }
 }
}
