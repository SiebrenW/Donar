import com.siemens.ct.exi.core.EXIFactory;
import com.siemens.ct.exi.core.FidelityOptions;
import com.siemens.ct.exi.core.grammars.Grammars;
import com.siemens.ct.exi.core.helpers.DefaultEXIFactory;
import com.siemens.ct.exi.grammars.GrammarFactory;
import com.siemens.ct.exi.main.api.sax.EXIResult;
import com.siemens.ct.exi.main.api.sax.EXISource;
import java.io.*;
import java.nio.file.*;
import java.util.*;
import org.xml.sax.*;
import org.xml.sax.helpers.AttributesImpl;

/** Decodes every document of the corpus with EXIficient (schema-informed, as the vectors were made:
 *  default fidelity, valuePartitionCapacity 0) into SAX events, replays the events into an encoder and
 *  compares the bytes. Prints the lines the other harnesses print.
 *  Usage: Bench corpus.txt manifest.txt schemas-dir */
public class Bench {
  /** One SAX event, kept for the replay. */
  record Event(int kind, String a, String b, String c, Attributes atts, char[] text) {}

  static class Recorder extends org.xml.sax.helpers.DefaultHandler {
    final List<Event> events = new ArrayList<>();
    public void startDocument() { events.add(new Event(0, null, null, null, null, null)); }
    public void endDocument() { events.add(new Event(1, null, null, null, null, null)); }
    public void startPrefixMapping(String p, String u) { events.add(new Event(2, p, u, null, null, null)); }
    public void endPrefixMapping(String p) { events.add(new Event(3, p, null, null, null, null)); }
    public void startElement(String u, String l, String q, Attributes at) { events.add(new Event(4, u, l, q, new AttributesImpl(at), null)); }
    public void endElement(String u, String l, String q) { events.add(new Event(5, u, l, q, null, null)); }
    public void characters(char[] ch, int s, int n) { events.add(new Event(6, null, null, null, null, Arrays.copyOfRange(ch, s, s + n))); }
  }

  static void replay(List<Event> events, ContentHandler h) throws SAXException {
    for (Event e : events) {
      switch (e.kind) {
        case 0 -> h.startDocument();
        case 1 -> h.endDocument();
        case 2 -> h.startPrefixMapping(e.a, e.b);
        case 3 -> h.endPrefixMapping(e.a);
        case 4 -> h.startElement(e.a, e.b, e.c, e.atts);
        case 5 -> h.endElement(e.a, e.b, e.c);
        default -> h.characters(e.text, 0, e.text.length);
      }
    }
  }

  static final Map<String, EXIFactory> factories = new HashMap<>();

  static EXIFactory factory(String xsd) throws Exception {
    EXIFactory f = factories.get(xsd);
    if (f == null) {
      f = DefaultEXIFactory.newInstance();
      Grammars g = GrammarFactory.newInstance().createGrammars(xsd);
      f.setGrammars(g);
      f.setValuePartitionCapacity(0);
      f.setFidelityOptions(FidelityOptions.createDefault());
      factories.put(xsd, f);
    }
    return f;
  }

  static List<Event> decode(EXIFactory f, byte[] bytes) throws Exception {
    EXISource src = new EXISource(f);
    XMLReader xr = src.getXMLReader();
    Recorder r = new Recorder();
    xr.setContentHandler(r);
    xr.parse(new InputSource(new ByteArrayInputStream(bytes)));
    return r.events;
  }

  static byte[] encode(EXIFactory f, List<Event> events) throws Exception {
    ByteArrayOutputStream out = new ByteArrayOutputStream(256);
    EXIResult r = new EXIResult(f);
    r.setOutputStream(out);
    replay(events, r.getHandler());
    return out.toByteArray();
  }

  static byte[] hex(String s) {
    byte[] b = new byte[s.length() / 2];
    for (int i = 0; i < b.length; i++) b[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
    return b;
  }

  static volatile Object sink;

  /** Nanoseconds per call: about 20 ms after a warm-up that lets the JIT see the code. */
  static double time(Callable c) throws Exception {
    for (int i = 0; i < 200; i++) sink = c.call();
    long t0 = System.nanoTime(), t1;
    long calls = 0;
    do {
      for (int i = 0; i < 16; i++) sink = c.call();
      calls += 16;
      t1 = System.nanoTime();
    } while (t1 - t0 < 20_000_000L);
    return (double) (t1 - t0) / calls;
  }

  interface Callable { Object call() throws Exception; }

  public static void main(String[] a) throws Exception {
    Map<String, String> xsdOf = new HashMap<>();
    for (String line : Files.readAllLines(Path.of(a[1]))) {
      if (line.startsWith("#") || line.isBlank()) continue;
      String[] f = line.split("\\s+");
      xsdOf.put(f[0], f[1]);
    }
    Map<String, double[]> tally = new LinkedHashMap<>(); // group -> ok, total, decode, encode
    for (String line : Files.readAllLines(Path.of(a[0]))) {
      String[] f = line.split("\t");
      if (f.length < 4) continue;
      String name = f[0], group = f[1];
      double[] t = tally.computeIfAbsent(group, g -> new double[4]);
      t[1]++;
      String xsd = xsdOf.get(name);
      if (xsd == null) { System.out.println("MISMATCH\texificient\t" + group + "\t" + name + "\tnot in the manifest"); continue; }
      xsd = a[2] + "/" + xsd;
      byte[] in = hex(f[2]);
      EXIFactory fac = factory(xsd);
      try {
        List<Event> events = decode(fac, in);
        byte[] again = encode(fac, events);
        if (!Arrays.equals(in, again)) { System.out.println("MISMATCH\texificient\t" + group + "\t" + name + "\tre-encoded differently"); continue; }
        t[2] += time(() -> decode(fac, in));
        t[3] += time(() -> encode(fac, events));
        t[0]++;
      } catch (Exception e) {
        System.out.println("MISMATCH\texificient\t" + group + "\t" + name + "\t" + e);
      }
    }
    for (var e : tally.entrySet()) {
      double[] t = e.getValue();
      double n = Math.max(1, t[0]);
      System.out.printf(Locale.ROOT, "RESULT\texificient\t%s\t%d\t%d\t%.1f\t%.1f\t0%n", e.getKey(), (int) t[0], (int) t[1], t[2] / n, t[3] / n);
    }
  }
}
