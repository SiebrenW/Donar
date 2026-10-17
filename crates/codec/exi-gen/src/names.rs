//! Rust identifiers from XSD names.

/// Splits `EVMaximumChargePower`, `OK_NewSession`, `PnC` into words:
/// `[EV, Maximum, Charge, Power]`, `[OK, New, Session]`, `[Pn, C]`.
pub fn words(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' || c == '.' || c == '/' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = match prev {
            Some(p) if p != '_' => {
                (p.is_lowercase() && c.is_uppercase())
                    // `X509Issuer` is two words, `V2XEnergy` is not three.
                    || (p.is_ascii_digit() && c.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
                    || (p.is_uppercase()
                        && c.is_uppercase()
                        && next.is_some_and(|n| n.is_lowercase())
                        && !plural_s(&chars, i + 1))
            }
            _ => false,
        };
        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `IDs`, `EVs`: a lone `s` after an acronym is a plural, not the start of a word.
fn plural_s(chars: &[char], at: usize) -> bool {
    chars[at] == 's' && chars.get(at + 1).is_none_or(|c| c.is_uppercase() || *c == '_')
}

fn capitalize(w: &str) -> String {
    let mut it = w.chars();
    match it.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &it.as_str().to_lowercase(),
        None => String::new(),
    }
}

pub fn pascal_words(ws: &[String]) -> String {
    ws.iter().map(|w| capitalize(w)).collect()
}

pub fn pascal(s: &str) -> String {
    let p = pascal_words(&words(s));
    if p.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("V{p}") } else { p }
}

pub fn snake_words(ws: &[String]) -> String {
    let s = ws.iter().map(|w| w.to_lowercase()).collect::<Vec<_>>().join("_");
    const KEYWORDS: &[&str] = &["type", "match", "loop", "move", "ref", "self", "mod", "use", "fn", "as", "box"];
    if KEYWORDS.contains(&s.as_str()) { format!("r#{s}") } else { s }
}

pub fn snake(s: &str) -> String {
    snake_words(&words(s))
}

/// The Rust type name of an XSD type: `fooBarType` / `FooBarType` -> `FooBar`.
pub fn type_name(local: &str) -> String {
    pascal(local.strip_suffix("Type").unwrap_or(local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_at_case_changes_and_underscores() {
        assert_eq!(words("EVMaximumChargePower"), ["EV", "Maximum", "Charge", "Power"]);
        assert_eq!(words("OK_NewSessionEstablished"), ["OK", "New", "Session", "Established"]);
        assert_eq!(words("CPDReq"), ["CPD", "Req"]);
        assert_eq!(words("SupportedServiceIDs"), ["Supported", "Service", "IDs"]);
    }

    #[test]
    fn identifiers() {
        assert_eq!(type_name("responseCodeType"), "ResponseCode");
        assert_eq!(type_name("DC_CPDReqEnergyTransferModeType"), "DcCpdReqEnergyTransferMode");
        assert_eq!(pascal("FAILED_UnknownSession"), "FailedUnknownSession");
        assert_eq!(snake("SessionID"), "session_id");
        assert_eq!(snake("Type"), "r#type");
    }
}
