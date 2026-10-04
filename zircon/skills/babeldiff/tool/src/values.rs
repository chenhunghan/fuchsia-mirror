//! Named constants the changed files define, so a value check can compare
//! what `PIC1` and `PIC1_COMMAND` stand for rather than how they are
//! spelled, and the words of the C++ comments in those files, so a Rust
//! comment the conversion added can be told from one it carried over.

use std::collections::{HashMap, HashSet};

/// Constants and statics defined by the changed C++ and Rust files.
#[derive(Clone, Debug, Default)]
pub struct Values {
    /// Constant name (as [`key`] spells it) to its defining expression.
    defs: HashMap<String, String>,
    /// Rust `static`s and C++ non-constant globals: state, not values.
    statics: HashSet<String>,
    /// Words of the C++ comments, and runs of three of them.
    comment_words: HashSet<String>,
    comment_triples: HashSet<String>,
}

/// How a value check spells a constant: `kMaxSize` as `MAX_SIZE`, anything
/// else as written.
pub fn key(raw: &str) -> String {
    match raw.strip_prefix('k') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_uppercase()) => {
            crate::normalize::ident(rest).to_ascii_uppercase()
        }
        _ => raw.to_string(),
    }
}

impl Values {
    /// Collects definitions from C++ and Rust source.
    pub fn add_cpp(&mut self, text: &str) {
        static DEFINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?m)^[ \t]*#[ \t]*define[ \t]+([A-Za-z_]\w*)[ \t]+([^\n]+)$")
                .unwrap()
        });
        static CONSTEXPR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"\b(?:constexpr|const)\s+[A-Za-z_][\w:<>, ]*?[\s*&]+([A-Za-z_]\w*)\s*(?:=\s*([^;{}]+)|\{\s*([^;{}]+)\})\s*;",
            )
            .unwrap()
        });
        static ENUM: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?m)^[ \t]*([A-Za-z_]\w*)[ \t]*=[ \t]*([^,;\n{}=]+?)[ \t]*,")
                .unwrap()
        });
        for c in DEFINE.captures_iter(text) {
            let body = c[2].split("//").next().unwrap_or("").trim();
            if !body.is_empty() && !body.ends_with('\\') {
                self.def(&c[1], body);
            }
        }
        for c in CONSTEXPR.captures_iter(text) {
            if let Some(e) = c.get(2).or(c.get(3)) {
                self.def(&c[1], e.as_str());
            }
        }
        for c in ENUM.captures_iter(text) {
            self.def(&c[1], &c[2]);
        }
        static COMMENT: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"//[^\n]*|/\*(?s:.*?)\*/").unwrap());
        let words: Vec<String> = COMMENT
            .find_iter(text)
            .flat_map(|m| crate::normalize::comment_words(m.as_str()))
            .collect();
        for w in words.windows(3) {
            self.comment_triples.insert(w.join(" "));
        }
        self.comment_words.extend(words);
    }

    /// Whether a comment's words come from the C++ comments: at least half
    /// of its runs of three words, or every word of a shorter comment.
    pub fn in_cpp_comments(&self, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        if words.len() < 3 {
            return words.iter().all(|w| self.comment_words.contains(w));
        }
        let hits = words
            .windows(3)
            .filter(|w| self.comment_triples.contains(&w.join(" ")))
            .count();
        2 * hits >= words.len() - 2
    }

    pub fn add_rust(&mut self, text: &str) {
        static CONST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\bconst\s+([A-Za-z_]\w*)\s*:[^=;]+=\s*([^;]+);").unwrap()
        });
        static STATIC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\bstatic\s+(?:mut\s+)?([A-Za-z_]\w*)\s*:").unwrap()
        });
        for c in CONST.captures_iter(text) {
            self.def(&c[1], &c[2]);
        }
        for c in STATIC.captures_iter(text) {
            self.statics.insert(key(&c[1]));
        }
    }

    fn def(&mut self, name: &str, expr: &str) {
        self.defs
            .entry(key(name))
            .or_insert_with(|| expr.trim().to_string());
    }

    /// Whether a changed file defines the name as a constant.
    pub fn defined(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }

    /// Whether the name is a `static`, which holds state rather than a value.
    pub fn is_static(&self, name: &str) -> bool {
        self.statics.contains(name) && !self.defs.contains_key(name)
    }

    /// The constant a name aliases, following `kFoo = ZX_FOO` chains.
    pub fn canonical(&self, name: &str) -> String {
        let mut n = name.to_string();
        for _ in 0..8 {
            match self.defs.get(&n).and_then(|e| alias(e)) {
                Some(next) if next != n => n = next,
                _ => break,
            }
        }
        n
    }

    /// The numeric value of a constant, when its definition can be
    /// evaluated.
    pub fn value(&self, name: &str) -> Option<u64> {
        self.eval_name(name, 0)
    }

    fn eval_name(&self, name: &str, depth: usize) -> Option<u64> {
        if depth > 8 {
            return None;
        }
        let e = self.defs.get(name)?;
        let toks = tokens(e)?;
        let mut p = Parser {
            toks: &toks,
            pos: 0,
            values: self,
            depth,
        };
        let v = p.expr(0)?;
        (p.pos == toks.len()).then_some(v)
    }
}

/// The name an expression consists of, if that is all it is.
fn alias(expr: &str) -> Option<String> {
    let e = expr
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let last = e.rsplit("::").next()?.trim();
    let ok = !last.is_empty()
        && last.chars().all(|c| c.is_alphanumeric() || c == '_')
        && last.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && e.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    ok.then(|| key(last))
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(u64),
    Name(String),
    Op(&'static str),
}

fn tokens(e: &str) -> Option<Vec<Tok>> {
    static TOK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\s*(?:(0[xX][0-9A-Fa-f_]+|0[bB][01_]+|[0-9][0-9_]*)(?:[uUlLzZ]+|_?[ui](?:8|16|32|64|128|size))?|([A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*)|(<<|>>|[|&^~!+\-*()]))",
        )
        .unwrap()
    });
    let e = e.trim();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < e.len() {
        let c = TOK.captures(&e[pos..])?;
        let m = c.get(0)?;
        if m.start() != 0 || m.end() == 0 {
            return None;
        }
        pos += m.end();
        if let Some(n) = c.get(1) {
            let t = n.as_str().replace('_', "");
            let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                u64::from_str_radix(h, 16).ok()?
            } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                u64::from_str_radix(b, 2).ok()?
            } else {
                t.parse().ok()?
            };
            out.push(Tok::Num(v));
        } else if let Some(n) = c.get(2) {
            let n: String = n.as_str().chars().filter(|c| !c.is_whitespace()).collect();
            // `x as u64` is a cast, and so is `u64::from(x)` below.
            if out.last() == Some(&Tok::Name("as".into())) {
                out.pop();
                continue;
            }
            out.push(Tok::Name(n));
        } else if let Some(o) = c.get(3) {
            const OPS: &[&str] = &["<<", ">>", "|", "&", "^", "~", "!", "+", "-", "*", "(", ")"];
            out.push(Tok::Op(OPS.iter().find(|&&x| x == o.as_str())?));
        }
    }
    Some(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    values: &'a Values,
    depth: usize,
}

impl Parser<'_> {
    fn prec(op: &str) -> Option<u8> {
        Some(match op {
            "|" => 1,
            "^" => 2,
            "&" => 3,
            "<<" | ">>" => 4,
            "+" | "-" => 5,
            "*" => 6,
            _ => return None,
        })
    }

    fn expr(&mut self, min: u8) -> Option<u64> {
        let mut lhs = self.unary()?;
        while let Some(Tok::Op(op)) = self.toks.get(self.pos) {
            let Some(p) = Self::prec(op) else { break };
            if p < min {
                break;
            }
            self.pos += 1;
            let rhs = self.expr(p + 1)?;
            lhs = match *op {
                "|" => lhs | rhs,
                "^" => lhs ^ rhs,
                "&" => lhs & rhs,
                "<<" => lhs.checked_shl(u32::try_from(rhs).ok()?)?,
                ">>" => lhs.checked_shr(u32::try_from(rhs).ok()?)?,
                "+" => lhs.wrapping_add(rhs),
                "-" => lhs.wrapping_sub(rhs),
                _ => lhs.wrapping_mul(rhs),
            };
        }
        Some(lhs)
    }

    fn unary(&mut self) -> Option<u64> {
        let t = self.toks.get(self.pos)?.clone();
        self.pos += 1;
        match t {
            Tok::Num(v) => Some(v),
            Tok::Op("~") | Tok::Op("!") => Some(!self.unary()?),
            Tok::Op("-") => Some(self.unary()?.wrapping_neg()),
            Tok::Op("(") => {
                let v = self.expr(0)?;
                (self.toks.get(self.pos) == Some(&Tok::Op(")"))).then(|| self.pos += 1)?;
                Some(v)
            }
            Tok::Name(n) => {
                let last = n.rsplit("::").next().unwrap_or(&n).to_string();
                if self.toks.get(self.pos) == Some(&Tok::Op("(")) {
                    // `BIT(3)`, and conversions such as `u64::from(x)`.
                    self.pos += 1;
                    let v = self.expr(0)?;
                    (self.toks.get(self.pos) == Some(&Tok::Op(")"))).then(|| self.pos += 1)?;
                    return match last.as_str() {
                        "BIT" | "bit" => 1u64.checked_shl(u32::try_from(v).ok()?),
                        "from" => Some(v),
                        _ => None,
                    };
                }
                self.values.eval_name(&key(&last), self.depth + 1)
            }
            Tok::Op(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_definitions() {
        let mut v = Values::default();
        v.add_cpp(
            "#define PIC1 0x20 // command port\n\
             constexpr uint32_t kMaxHandles = ZX_WAIT_MANY_MAX_ITEMS;\n\
             static constexpr uint64_t kMask{BIT(3) | 1u};\n\
             enum Color {\n  RED = 1,\n  BLUE = RED << 2,\n};\n",
        );
        v.add_rust(
            "pub const PIC1_COMMAND: u16 = 0x20;\n\
             const DR7_MASK: u64 = (1 << 8) | (1 << 9) | (1 << 10);\n\
             static mut HPET_STATE: State = State::new();\n",
        );
        assert_eq!(v.value("PIC1"), Some(0x20));
        assert_eq!(v.value("PIC1_COMMAND"), Some(0x20));
        assert_eq!(v.value("MASK"), Some(9));
        assert_eq!(v.value("BLUE"), Some(4));
        assert_eq!(v.value("DR7_MASK"), Some(0x700));
        assert_eq!(v.canonical("MAX_HANDLES"), "ZX_WAIT_MANY_MAX_ITEMS");
        assert!(v.is_static("HPET_STATE"));
        assert!(v.defined("MAX_HANDLES"));
    }
}
