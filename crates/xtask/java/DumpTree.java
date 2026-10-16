import com.siemens.ct.exi.core.grammars.Grammars;
import com.siemens.ct.exi.core.grammars.event.*;
import com.siemens.ct.exi.core.grammars.grammar.Grammar;
import com.siemens.ct.exi.core.grammars.production.Production;
import com.siemens.ct.exi.grammars.GrammarFactory;
import java.util.*;

/** Usage: DumpTree <xsd> <root local name>
 *  Prints the schema-informed grammar of one root element as a set of canonical states:
 *  structurally identical states are merged and numbered in the order they are completed
 *  (children before the state that refers to them). `exi-gen dump` prints the same format,
 *  so the two outputs can be diffed. xmldsig subtrees are not expanded. */
public class DumpTree {
  static final Map<Grammar, Integer> memo = new IdentityHashMap<>();
  static final Map<String, Integer> interned = new HashMap<>();
  static final List<String> lines = new ArrayList<>();

  static String name(javax.xml.namespace.QName q) {
    return q.getNamespaceURI().isEmpty() ? q.getLocalPart() : "{" + q.getNamespaceURI() + "}" + q.getLocalPart();
  }

  static int intern(String sig) {
    Integer id = interned.get(sig);
    if (id == null) { id = lines.size(); interned.put(sig, id); lines.add("S" + id + ": " + sig); }
    return id;
  }

  static int canon(Grammar g) {
    Integer known = memo.get(g);
    if (known != null) return known;
    StringBuilder sb = new StringBuilder();
    for (int i = 0; i < g.getNumberOfEvents(); i++) {
      Production p = g.getProduction(i);
      Event ev = p.getEvent();
      String d;
      if (ev instanceof StartElement && ((StartElement) ev).getQName() != null) {
        StartElement se = (StartElement) ev;
        String child;
        String ns = se.getQName().getNamespaceURI();
        String local = se.getQName().getLocalPart();
        // The xmldsig types exi-gen models (X509IssuerSerialType) are expanded.
        boolean modelled = local.equals("X509IssuerName") || local.equals("X509SerialNumber");
        if (ns.startsWith("http://www.w3.org/2000/09/xmldsig") && !modelled) child = "XMLDSIG";
        else child = "S" + canon(se.getGrammar());
        d = "SE(" + name(se.getQName()) + ") child=" + child;
      } else if (ev instanceof EndElement) d = "EE";
      else if (ev instanceof Characters) d = "CH:" + ((Characters) ev).getDatatype().getBuiltInType();
      else if (ev instanceof Attribute) d = "AT:" + name(((Attribute) ev).getQName()) + ":" + ((Attribute) ev).getDatatype().getBuiltInType();
      else d = ev.toString();
      String next = p.getNextGrammar() == null ? "end" : "S" + canon(p.getNextGrammar());
      sb.append(p.getEventCode()).append(' ').append(d).append(" ->").append(next).append(';');
    }
    int id = intern(sb.toString());
    memo.put(g, id);
    return id;
  }

  public static void main(String[] a) throws Exception {
    Grammars g = GrammarFactory.newInstance().createGrammars(a[0]);
    Grammar content = g.getDocumentGrammar().getProduction(0).getNextGrammar();
    for (int i = 0; i < content.getNumberOfEvents(); i++) {
      Event ev = content.getProduction(i).getEvent();
      if (ev instanceof StartElement && ((StartElement) ev).getQName() != null
          && ((StartElement) ev).getQName().getLocalPart().equals(a[1])) {
        int start = canon(((StartElement) ev).getGrammar());
        System.out.println("ROOT " + i + " " + name(((StartElement) ev).getQName()) + " count=" + content.getNumberOfEvents());
        for (String l : lines) System.out.println(l);
        System.out.println("START S" + start);
      }
    }
  }
}
