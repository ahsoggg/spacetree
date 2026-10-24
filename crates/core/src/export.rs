use std::io::{self, Write};

use crate::tree::{ROOT, Tree};

/// A cell that starts with one of these is a formula for Excel / LibreOffice (CSV injection, e.g. `=cmd|' /C calc'!A0`).
const FORMULA_START: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

fn field(w: &mut impl Write, s: &str) -> io::Result<()> {
    // OWASP: a leading apostrophe makes spreadsheets read the cell as text.
    let guard = if s.starts_with(FORMULA_START) { "'" } else { "" };
    if guard.is_empty() && !s.contains([',', '"', '\n', '\r']) {
        w.write_all(s.as_bytes())
    } else {
        write!(w, "\"{guard}{}\"", s.replace('"', "\"\""))
    }
}

/// Writes `path,size,size_on_disk,files` for every node (depth-first, larger siblings first).
/// Directory paths end with a path separator.
pub fn write_csv(tree: &Tree, w: &mut impl Write) -> io::Result<()> {
    writeln!(w, "path,size,size_on_disk,files")?;
    let sep = std::path::MAIN_SEPARATOR;
    let mut path = String::new();
    let mut stack: Vec<(u32, usize)> = vec![(ROOT, 0)];
    while let Some((id, plen)) = stack.pop() {
        path.truncate(plen);
        path.push_str(tree.name(id));
        let is_dir = tree.is_dir(id);
        if is_dir && !path.ends_with(sep) {
            path.push(sep);
        }
        let nd = tree.node(id);
        field(w, &path)?;
        writeln!(w, ",{},{},{}", nd.size, nd.alloc, nd.files)?;
        if is_dir {
            let l = path.len();
            for &c in tree.children(id).iter().rev() {
                stack.push((c, l));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(s: &str) -> String {
        let mut v = Vec::new();
        field(&mut v, s).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn formula_cells_are_neutralised() {
        assert_eq!(cell("=cmd|' /C calc'!A0"), "\"'=cmd|' /C calc'!A0\"");
        assert_eq!(cell("@SUM(1)"), "\"'@SUM(1)\"");
        assert_eq!(cell("-1+1"), "\"'-1+1\"");
        assert_eq!(cell("+1"), "\"'+1\"");
    }

    #[test]
    fn ordinary_cells_are_untouched() {
        assert_eq!(cell(r"C:\a=b\x"), r"C:\a=b\x");
        assert_eq!(cell("a,b"), "\"a,b\"");
        assert_eq!(cell("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_of_a_hostile_root_name() {
        let mut b = crate::tree::Builder::default();
        let root = b.add(
            crate::tree::NO_PARENT,
            "=HYPERLINK(\"x\")",
            crate::tree::Kind::Dir,
            0,
            0,
        );
        b.add(root, "f", crate::tree::Kind::File, 1, 1);
        let mut out = Vec::new();
        write_csv(&b.finish(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.lines().skip(1).all(|l| !l.starts_with('=')), "{text}");
    }
}
