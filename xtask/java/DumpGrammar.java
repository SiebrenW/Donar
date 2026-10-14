import com.siemens.ct.exi.core.grammars.Grammars;
import com.siemens.ct.exi.core.grammars.event.StartElement;
import com.siemens.ct.exi.core.grammars.grammar.Grammar;
import com.siemens.ct.exi.core.grammars.production.Production;
import com.siemens.ct.exi.grammars.GrammarFactory;

/** Usage: DumpGrammar <xsd> <root local name> [path of child local names...]
 *  Prints the productions of the element grammar reached by following the first-level
 *  productions named by the path (each step enters that child element's grammar). */
public class DumpGrammar {
  static void dump(String indent, Grammar g) {
    System.out.println(indent + "type=" + g.getGrammarType() + " events=" + g.getNumberOfEvents() + " hasEE=" + g.hasEndElement());
    for (int i = 0; i < g.getNumberOfEvents(); i++) {
      Production p = g.getProduction(i);
      System.out.println(indent + "  " + p.getEventCode() + " " + p.getEvent());
    }
  }
  public static void main(String[] a) throws Exception {
    Grammars g = GrammarFactory.newInstance().createGrammars(a[0]);
    Grammar content = g.getDocumentGrammar().getProduction(0).getNextGrammar();
    Grammar cur = null;
    for (int i = 0; i < content.getNumberOfEvents(); i++) {
      Object ev = content.getProduction(i).getEvent();
      if (ev instanceof StartElement && ((StartElement) ev).getQName().getLocalPart().equals(a[1])) cur = ((StartElement) ev).getGrammar();
    }
    dump("", cur);
    for (int k = 2; k < a.length; k++) {
      Grammar next = null;
      // walk forward along productions until the named child start element is found
      Grammar walk = cur;
      outer:
      while (walk != null) {
        for (int i = 0; i < walk.getNumberOfEvents(); i++) {
          Production p = walk.getProduction(i);
          Object ev = p.getEvent();
          if (ev instanceof StartElement && ((StartElement) ev).getQName() != null && ((StartElement) ev).getQName().getLocalPart().equals(a[k])) {
            next = ((StartElement) ev).getGrammar(); break outer;
          }
        }
        walk = walk.getNumberOfEvents() > 0 ? walk.getProduction(0).getNextGrammar() : null;
      }
      System.out.println("== " + a[k]);
      cur = next;
      dump("", cur);
    }
  }
}
