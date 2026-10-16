import com.siemens.ct.exi.core.grammars.Grammars;
import com.siemens.ct.exi.core.grammars.grammar.Grammar;
import com.siemens.ct.exi.grammars.GrammarFactory;

/** Usage: DumpRoots <xsd> -> lists the document-content productions in event-code order. */
public class DumpRoots {
  public static void main(String[] a) throws Exception {
    Grammars g = GrammarFactory.newInstance().createGrammars(a[0]);
    Grammar content = g.getDocumentGrammar().getProduction(0).getNextGrammar();
    for (int i = 0; i < content.getNumberOfEvents(); i++)
      System.out.println(i + " " + content.getProduction(i).getEvent());
  }
}
