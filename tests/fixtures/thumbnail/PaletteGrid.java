import java.awt.image.*; import java.awt.*; import java.nio.file.*; import java.nio.*;
public class PaletteGrid {
 public static void main(String[] a) throws Exception {
  for(int mode=0;mode<10;mode++) {
   int n=mode<3?256:(mode==3?8:(mode==4?7:(mode==5?2:mode==9?5:262-mode)));byte[] r=new byte[n],g=new byte[n],b=new byte[n];
   for(int i=0;i<n;i++) {
    int c=mode==0?i:mode==1?255-i:mode==2?(i*73)&255:((i&4)>0?255:0);
    r[i]=(byte)c;g[i]=(byte)(mode<3?c:((i&2)>0?255:0));b[i]=(byte)(mode<3?c:((i&1)>0?255:0));
   }
   if(mode>=6)for(int i=0;i<n;i++){int v=i<248?i:i-248;r[i]=(byte)(i<248?v:((v&4)>0?255:0));g[i]=(byte)(i<248?v:((v&2)>0?255:0));b[i]=(byte)(i<248?v:((v&1)>0?255:0));}
   if(mode==9){int[] v={0,9,9,11,255};for(int i=0;i<n;i++)r[i]=g[i]=b[i]=(byte)v[i];}
   IndexColorModel cm=new IndexColorModel(8,n,r,g,b); BufferedImage src=new BufferedImage(2048,8,BufferedImage.TYPE_INT_RGB);
   for(int y=0;y<8;y++)for(int x=0;x<2048;x++)src.setRGB(x,y,0xff000000|((x/8)*0x010101));
   BufferedImage dst=new BufferedImage(cm,cm.createCompatibleWritableRaster(2048,8),false,null);Graphics gg=dst.getGraphics();gg.drawImage(src,0,0,null);gg.dispose();
   Files.write(Path.of(a[0],"palette-grid-"+mode+".bin"),((DataBufferByte)dst.getRaster().getDataBuffer()).getData());
   ByteBuffer pal=ByteBuffer.allocate(n*4).order(ByteOrder.LITTLE_ENDIAN);for(int i=0;i<n;i++)pal.putInt(cm.getRGB(i));Files.write(Path.of(a[0],"palette-grid-"+mode+".pal"),pal.array());
  }
 }
}
