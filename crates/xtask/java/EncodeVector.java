import com.siemens.ct.exi.core.EXIFactory;
import com.siemens.ct.exi.core.FidelityOptions;
import com.siemens.ct.exi.core.helpers.DefaultEXIFactory;
import com.siemens.ct.exi.core.grammars.Grammars;
import com.siemens.ct.exi.core.EncodingOptions;
import com.siemens.ct.exi.grammars.GrammarFactory;
import com.siemens.ct.exi.main.api.sax.EXIResult;
import java.io.*;
import javax.xml.parsers.SAXParserFactory;
import org.xml.sax.*;

/** Usage: EncodeVector <xsd> <strict|lax> <header|noheader> <in.xml> [fragment]  -> prints EXI as hex.
 *  With `fragment` the document is one element encoded with the schema-informed fragment grammar. */
public class EncodeVector {
  public static void main(String[] a) throws Exception {
    EXIFactory f = DefaultEXIFactory.newInstance();
    Grammars g = GrammarFactory.newInstance().createGrammars(a[0]);
    f.setGrammars(g);
    if (a.length > 4 && a[4].equals("fragment")) f.setFragment(true);
    // DIN SPEC 70121 (V2G-DC-176): valuePartitionCapacity 0.
    if (a[0].contains("din-spec")) f.setValuePartitionCapacity(0);
    f.setFidelityOptions(a[1].equals("strict") ? FidelityOptions.createStrict() : FidelityOptions.createDefault());
    EncodingOptions eo = EncodingOptions.createDefault();
    if (a[2].equals("header")) eo.setOption(EncodingOptions.INCLUDE_OPTIONS);
    f.setEncodingOptions(eo);
    ByteArrayOutputStream out = new ByteArrayOutputStream();
    EXIResult r = new EXIResult(f);
    r.setOutputStream(out);
    SAXParserFactory spf = SAXParserFactory.newInstance();
    spf.setNamespaceAware(true);
    XMLReader xr = spf.newSAXParser().getXMLReader();
    xr.setContentHandler(r.getHandler());
    xr.parse(new InputSource(new FileInputStream(a[3])));
    StringBuilder sb = new StringBuilder();
    for (byte b : out.toByteArray()) sb.append(String.format("%02x", b));
    System.out.println(sb);
  }
}
