//! Value-position refs (bead `ley-line-open-a9ff82`).
//!
//! An identifier used as a VALUE — assigned, passed as an argument,
//! returned, placed in a composite literal, or taken as a method value —
//! must record a `node_refs` row resolving to its def. Before this bead
//! only call targets (and, for Go, bare identifiers in an argument list
//! or a keyed composite-literal value) emitted, so a handler passed by
//! value (`var h = helper`, `cobra.Command{RunE: run}`,
//! `http.HandleFunc("/", s.serve)`) read as dead in mache's `dead_code`.
//!
//! Every fixture runs through `parse_into_conn` end-to-end, so grammar
//! wiring, the compiled `tags.scm`, language dispatch and the batch
//! insert are all on the path.
//!
//! Value refs are told apart from call refs by the occurrence's own
//! grammar kind (`node_refs.node_kind`, and `kind_id` → `kinds.raw_kind`):
//! a call ref is anchored at the call node, a value ref at the
//! value-position container (`argument_list`, `var_spec`,
//! `return_statement`, ...). The tests pin that kind per row.

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use tempfile::TempDir;

/// One `node_refs` row as consumers see it: token, structural
/// qualifier, the occurrence's grammar kind.
type Row = (String, Option<String>, String);

fn cold_parse(src_dir: &Path, lang: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    leyline_cli_lib::cmd_parse::parse_into_conn(&conn, src_dir, Some(lang), None)
        .expect("parse_into_conn must succeed");
    conn
}

fn parse_one(lang: &str, rel: &str, src: &str) -> Connection {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(rel), src).unwrap();
    cold_parse(dir.path(), lang)
}

/// Every `node_refs` row of `rel`, sorted, with whether its token
/// resolves to a `node_defs` row.
fn ref_rows(conn: &Connection, rel: &str) -> Vec<(Row, bool)> {
    let file_id = leyline_ts::schema::lookup_file_id(conn, rel)
        .expect("file_id lookup")
        .unwrap_or_else(|| panic!("{rel} was never interned — the parse did not see it"));
    let (lo, hi) = leyline_ts::schema::file_nid_range(file_id);
    conn.prepare(
        "SELECT r.token, r.qualifier, k.raw_kind,
                EXISTS (SELECT 1 FROM node_defs d WHERE d.token = r.token)
           FROM node_refs r JOIN kinds k ON k.kind_id = r.kind_id
          WHERE r.nid BETWEEN ?1 AND ?2
          ORDER BY r.token, r.qualifier, k.raw_kind",
    )
    .unwrap()
    .query_map([lo, hi], |r| {
        Ok(((r.get(0)?, r.get(1)?, r.get(2)?), r.get::<_, i64>(3)? != 0))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn row(token: &str, qualifier: Option<&str>, kind: &str) -> Row {
    (token.into(), qualifier.map(Into::into), kind.into())
}

/// The qualified-token half of a dual-emit through a receiver VARIABLE
/// (`s.M`, `self.meth`, `this.m`): present, but no def carries that
/// token — its bare companion (`M` with qualifier `s`) is the row that
/// resolves, exactly as for a call through the same receiver.
fn receiver_row(token: &str, kind: &str) -> Row {
    (token.into(), None, format!("{RECEIVER}{kind}"))
}

const RECEIVER: &str = "receiver:";

/// Each expected value-position row is present AND resolves to a def,
/// except `receiver_row`s, which are present and do not resolve.
fn assert_value_refs_resolve(lang: &str, rel: &str, src: &str, expected: &[Row]) {
    let conn = parse_one(lang, rel, src);
    let rows = ref_rows(&conn, rel);
    for want in expected {
        let (want, must_resolve) = match want.2.strip_prefix(RECEIVER) {
            Some(kind) => ((want.0.clone(), want.1.clone(), kind.to_string()), false),
            None => (want.clone(), true),
        };
        let hit = rows.iter().find(|(r, _)| *r == want);
        match hit {
            None => panic!("{lang}: missing value-position ref {want:?}; got {rows:#?}"),
            Some((_, resolves)) => assert_eq!(
                *resolves, must_resolve,
                "{lang}: value-position ref {want:?} resolution to a node_defs row"
            ),
        }
    }
}

/// The exact ref multiset of a CALL-ONLY fixture. Pinned against the
/// pre-bead producer: value-position extraction adds rows only where a
/// value-position identifier exists, so these stay byte-for-byte equal.
fn assert_call_only_unchanged(lang: &str, rel: &str, src: &str, expected: &[Row]) {
    let conn = parse_one(lang, rel, src);
    let got: Vec<Row> = ref_rows(&conn, rel).into_iter().map(|(r, _)| r).collect();
    let mut want = expected.to_vec();
    want.sort();
    assert_eq!(got, want, "{lang}: call-only ref rows changed");
}

// ── mache's probe ───────────────────────────────────────────────────────

/// mache's exact probe (v0.27.0): one call ref, no value ref. After the
/// fix, `helper` has exactly two refs — the call and the `var` value.
#[test]
fn mache_probe_go_var_value_yields_two_helper_refs() {
    let conn = parse_one(
        "go",
        "probe.go",
        "package m\nfunc helper(){}; func Add(){ return helper(x) }; var h = helper\n",
    );
    let helper: Vec<Row> = ref_rows(&conn, "probe.go")
        .into_iter()
        .filter(|((t, _, _), _)| t == "helper")
        .map(|(r, resolves)| {
            assert!(resolves, "helper ref must resolve to its def");
            r
        })
        .collect();
    assert_eq!(
        helper,
        vec![
            row("helper", None, "call_expression"),
            row("helper", None, "var_spec"),
        ],
        "mache probe: helper must have exactly the call ref and the var value ref"
    );
}

// ── per-language value-position fixtures ───────────────────────────────

#[test]
fn go_function_used_only_as_value_resolves() {
    let src = r#"package m

type T struct{ fn func() }
type S struct{}

func (S) M() {}

func byVar()    {}
func byShort()  {}
func byAssign() {}
func byArg()    {}
func byReturn() {}
func byKeyed()  {}
func byElem()   {}
func register(f func()) {}

var h = byVar

func wire(s S) func() {
	x := byShort
	var y func()
	y = byAssign
	register(byArg)
	register(s.M)
	_ = T{fn: byKeyed}
	_ = []func(){byElem}
	_, _ = x, y
	return byReturn
}
"#;
    assert_value_refs_resolve(
        "go",
        "value.go",
        src,
        &[
            row("byVar", None, "var_spec"),
            row("byShort", None, "short_var_declaration"),
            row("byAssign", None, "assignment_statement"),
            row("byArg", None, "argument_list"),
            row("M", Some("s"), "argument_list"),
            receiver_row("s.M", "argument_list"),
            row("byKeyed", None, "keyed_element"),
            row("byElem", None, "literal_value"),
            row("byReturn", None, "return_statement"),
        ],
    );
}

#[test]
fn rust_function_used_only_as_value_resolves() {
    let src = r#"struct S { f: fn() }
struct H;
impl H { fn assoc() {} }
fn by_let() {}
fn by_assign() {}
fn by_arg() {}
fn by_field() {}
fn by_array() {}
fn by_return() {}
fn by_tail() {}
fn reg(_: fn()) {}
fn wire(c: bool) -> fn() {
    let a = by_let;
    let mut b: fn() = a;
    b = by_assign;
    reg(by_arg);
    reg(H::assoc);
    let _s = S { f: by_field };
    let _v = [by_array];
    if c { return by_return; }
    by_tail
}
"#;
    assert_value_refs_resolve(
        "rust",
        "value.rs",
        src,
        &[
            row("by_let", None, "let_declaration"),
            row("by_assign", None, "assignment_expression"),
            row("by_arg", None, "arguments"),
            row("H::assoc", None, "arguments"),
            row("assoc", Some("H"), "arguments"),
            row("by_field", None, "field_initializer"),
            row("by_array", None, "array_expression"),
            row("by_return", None, "return_expression"),
            row("by_tail", None, "block"),
        ],
    );
}

#[test]
fn python_function_used_only_as_value_resolves() {
    let src = r#"def by_assign(): pass
def by_arg(): pass
def by_kwarg(): pass
def by_return(): pass
def by_dict(): pass
def by_list(): pass
def by_deco(f): return f

class C:
    def meth(self): pass
    def wire(self):
        register(self.meth)

h = by_assign
register(by_arg)
Thread(target=by_kwarg)
table = {"k": by_dict}
items = [by_list]

@by_deco
def decorated(): pass

def factory():
    return by_return
"#;
    assert_value_refs_resolve(
        "python",
        "value.py",
        src,
        &[
            row("by_assign", None, "assignment"),
            row("by_arg", None, "argument_list"),
            receiver_row("self.meth", "argument_list"),
            row("meth", Some("self"), "argument_list"),
            row("by_kwarg", None, "keyword_argument"),
            row("by_dict", None, "pair"),
            row("by_list", None, "list"),
            row("by_deco", None, "decorator"),
            row("by_return", None, "return_statement"),
        ],
    );
}

const JS_VALUE_SRC: &str = r#"function byConst() {}
function byAssign() {}
function byArg() {}
function byPair() {}
function byShorthand() {}
function byArray() {}
function byReturn() {}
function byJsx() {}
function byExport() {}
class C { m() {} wire() { register(this.m); } }
const h = byConst;
let x; x = byAssign;
register(byArg);
const o = { k: byPair, byShorthand };
const l = [byArray];
function factory() { return byReturn; }
const e = <B onClick={byJsx} />;
export default byExport;
"#;

fn js_value_rows() -> Vec<Row> {
    vec![
        row("byConst", None, "variable_declarator"),
        row("byAssign", None, "assignment_expression"),
        row("byArg", None, "arguments"),
        receiver_row("this.m", "arguments"),
        row("m", Some("this"), "arguments"),
        row("byPair", None, "pair"),
        row("byShorthand", None, "object"),
        row("byArray", None, "array"),
        row("byReturn", None, "return_statement"),
        row("byJsx", None, "jsx_expression"),
        row("byExport", None, "export_statement"),
    ]
}

#[test]
fn javascript_function_used_only_as_value_resolves() {
    assert_value_refs_resolve("javascript", "value.js", JS_VALUE_SRC, &js_value_rows());
}

#[test]
fn typescript_function_used_only_as_value_resolves() {
    assert_value_refs_resolve("typescript", "value.tsx", JS_VALUE_SRC, &js_value_rows());
}

#[test]
fn java_method_used_only_as_value_resolves() {
    // Java methods are not first-class: the value form is the method
    // reference. Tokens join on `.` like Java's receiver invocations,
    // so `A::staticTarget` resolves against the `A.staticTarget` def.
    let src = r#"class A {
    void target() {}
    static void staticTarget() {}
    void run(Runnable r) {}
    void wire() {
        Runnable r = this::target;
        run(A::staticTarget);
    }
}
"#;
    assert_value_refs_resolve(
        "java",
        "A.java",
        src,
        &[
            receiver_row("this.target", "method_reference"),
            row("target", Some("this"), "method_reference"),
            row("A.staticTarget", None, "method_reference"),
            row("staticTarget", Some("A"), "method_reference"),
        ],
    );
}

const C_VALUE_SRC: &str = r#"void by_init(void) {}
void by_assign(void) {}
void by_arg(void) {}
void by_addr(void) {}
void by_return(void) {}
void by_designated(void) {}
void by_elem(void) {}
typedef void (*fn)(void);
struct ops { fn a; fn b; };
void reg(fn f) {}
fn h = by_init;
struct ops table = { .a = by_designated, by_elem };
fn wire(void) {
    fn x;
    x = by_assign;
    reg(by_arg);
    reg(&by_addr);
    return by_return;
}
"#;

fn c_value_rows() -> Vec<Row> {
    vec![
        row("by_init", None, "init_declarator"),
        row("by_assign", None, "assignment_expression"),
        row("by_arg", None, "argument_list"),
        row("by_addr", None, "pointer_expression"),
        row("by_return", None, "return_statement"),
        row("by_designated", None, "initializer_pair"),
        row("by_elem", None, "initializer_list"),
    ]
}

#[test]
fn c_function_used_only_as_value_resolves() {
    assert_value_refs_resolve("c", "value.c", C_VALUE_SRC, &c_value_rows());
}

#[test]
fn cpp_function_used_only_as_value_resolves() {
    let src = format!(
        "{C_VALUE_SRC}namespace ns {{ void f() {{}} void g() {{}} }}\n\
         void wire2() {{ fn y; reg(&ns::f); y = ns::g; }}\n"
    );
    let mut rows = c_value_rows();
    rows.extend([
        row("ns::f", None, "pointer_expression"),
        row("f", Some("ns"), "pointer_expression"),
        row("ns::g", None, "assignment_expression"),
        row("g", Some("ns"), "assignment_expression"),
    ]);
    assert_value_refs_resolve("cpp", "value.cpp", &src, &rows);
}

// ── call-only fixtures: rows unchanged ─────────────────────────────────

#[test]
fn go_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "go",
        "call.go",
        "package m\nimport \"fmt\"\nfunc helper() {}\nfunc main() {\n\thelper()\n\tfmt.Println(\"x\", 1)\n}\n",
        &[
            row("helper", None, "call_expression"),
            row("fmt.Println", None, "call_expression"),
            row("Println", Some("fmt"), "call_expression"),
        ],
    );
}

#[test]
fn rust_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "rust",
        "call.rs",
        "fn helper() {}\nfn main() {\n    helper();\n    std::process::exit(1);\n    println!(\"x\");\n}\n",
        &[
            row("helper", None, "call_expression"),
            row("std::process::exit", None, "call_expression"),
            row("exit", Some("std::process"), "call_expression"),
            row("println", None, "macro_invocation"),
        ],
    );
}

#[test]
fn python_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "python",
        "call.py",
        "def helper():\n    pass\n\nhelper()\nos.getcwd()\n",
        &[
            row("helper", None, "call"),
            row("os.getcwd", None, "call"),
            row("getcwd", Some("os"), "call"),
        ],
    );
}

#[test]
fn javascript_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "javascript",
        "call.js",
        "function helper() {}\nhelper();\nconsole.log(\"x\", 1);\n",
        &[
            row("helper", None, "call_expression"),
            row("console.log", None, "call_expression"),
            row("log", Some("console"), "call_expression"),
        ],
    );
}

#[test]
fn typescript_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "typescript",
        "call.ts",
        "function helper(): void {}\nhelper();\nconsole.log(\"x\", 1);\n",
        &[
            row("helper", None, "call_expression"),
            row("console.log", None, "call_expression"),
            row("log", Some("console"), "call_expression"),
        ],
    );
}

#[test]
fn java_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "java",
        "Call.java",
        "class Call {\n    void helper() {}\n    void main() {\n        helper();\n        System.out.println(\"x\");\n        new Call();\n    }\n}\n",
        &[
            row("helper", None, "method_invocation"),
            row("System.out.println", None, "method_invocation"),
            row("println", Some("System.out"), "method_invocation"),
            row("Call", None, "object_creation_expression"),
        ],
    );
}

#[test]
fn c_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "c",
        "call.c",
        "void helper(void) {}\nint main(void) {\n    helper();\n    printf(\"x\", 1);\n    return 0;\n}\n",
        &[
            row("helper", None, "call_expression"),
            row("printf", None, "call_expression"),
        ],
    );
}

#[test]
fn cpp_call_only_refs_unchanged() {
    assert_call_only_unchanged(
        "cpp",
        "call.cpp",
        "void helper() {}\nint main() {\n    helper();\n    std::exit(1);\n    return 0;\n}\n",
        &[
            row("helper", None, "call_expression"),
            row("std::exit", None, "call_expression"),
            row("exit", Some("std"), "call_expression"),
        ],
    );
}
