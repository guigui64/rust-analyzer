//! Collects diagnostics & fixits  for a single file.
//!
//! The tricky bit here is that diagnostics are produced by hir in terms of
//! macro-expanded files, but we need to present them to the users in terms of
//! original files. So we need to map the ranges.

use std::cell::RefCell;

use hir::{
    diagnostics::{AstDiagnostic, Diagnostic as _, DiagnosticSink},
    Semantics,
};
use itertools::Itertools;
use ra_db::{RelativePath, SourceDatabase, SourceDatabaseExt};
use ra_ide_db::RootDatabase;
use ra_prof::profile;
use ra_syntax::{
    algo,
    ast::{self, make, AstNode},
    SyntaxNode, TextRange, T,
};
use ra_text_edit::{TextEdit, TextEditBuilder};

use crate::{Diagnostic, FileId, FileSystemEdit, SourceChange, SourceFileEdit};

#[derive(Debug, Copy, Clone)]
pub enum Severity {
    Error,
    WeakWarning,
}

pub(crate) fn diagnostics(db: &RootDatabase, file_id: FileId) -> Vec<Diagnostic> {
    let _p = profile("diagnostics");
    let sema = Semantics::new(db);
    let parse = db.parse(file_id);
    let mut res = Vec::new();

    res.extend(parse.errors().iter().map(|err| Diagnostic {
        range: err.range(),
        message: format!("Syntax Error: {}", err),
        severity: Severity::Error,
        fix: None,
    }));

    for node in parse.tree().syntax().descendants() {
        check_unnecessary_braces_in_use_statement(&mut res, file_id, &node);
        check_struct_shorthand_initialization(&mut res, file_id, &node);
    }
    let res = RefCell::new(res);
    let mut sink = DiagnosticSink::new(|d| {
        res.borrow_mut().push(Diagnostic {
            message: d.message(),
            range: sema.diagnostics_range(d).range,
            severity: Severity::Error,
            fix: None,
        })
    })
    .on::<hir::diagnostics::UnresolvedModule, _>(|d| {
        let original_file = d.source().file_id.original_file(db);
        let source_root = db.file_source_root(original_file);
        let path = db
            .file_relative_path(original_file)
            .parent()
            .unwrap_or_else(|| RelativePath::new(""))
            .join(&d.candidate);
        let create_file = FileSystemEdit::CreateFile { source_root, path };
        let fix = SourceChange::file_system_edit("Create module", create_file);
        res.borrow_mut().push(Diagnostic {
            range: sema.diagnostics_range(d).range,
            message: d.message(),
            severity: Severity::Error,
            fix: Some(fix),
        })
    })
    .on::<hir::diagnostics::MissingFields, _>(|d| {
        // Note that although we could add a diagnostics to
        // fill the missing tuple field, e.g :
        // `struct A(usize);`
        // `let a = A { 0: () }`
        // but it is uncommon usage and it should not be encouraged.
        let fix = if d.missed_fields.iter().any(|it| it.as_tuple_index().is_some()) {
            None
        } else {
            let mut field_list = d.ast(db);
            for f in d.missed_fields.iter() {
                let field =
                    make::record_field(make::name_ref(&f.to_string()), Some(make::expr_unit()));
                field_list = field_list.append_field(&field);
            }

            let mut builder = TextEditBuilder::default();
            algo::diff(&d.ast(db).syntax(), &field_list.syntax()).into_text_edit(&mut builder);

            Some(SourceChange::source_file_edit_from(
                "Fill struct fields",
                file_id,
                builder.finish(),
            ))
        };

        res.borrow_mut().push(Diagnostic {
            range: sema.diagnostics_range(d).range,
            message: d.message(),
            severity: Severity::Error,
            fix,
        })
    })
    .on::<hir::diagnostics::MissingMatchArms, _>(|d| {
        res.borrow_mut().push(Diagnostic {
            range: sema.diagnostics_range(d).range,
            message: d.message(),
            severity: Severity::Error,
            fix: None,
        })
    })
    .on::<hir::diagnostics::MissingOkInTailExpr, _>(|d| {
        let node = d.ast(db);
        let replacement = format!("Ok({})", node.syntax());
        let edit = TextEdit::replace(node.syntax().text_range(), replacement);
        let fix = SourceChange::source_file_edit_from("Wrap with ok", file_id, edit);
        res.borrow_mut().push(Diagnostic {
            range: sema.diagnostics_range(d).range,
            message: d.message(),
            severity: Severity::Error,
            fix: Some(fix),
        })
    });
    if let Some(m) = sema.to_module_def(file_id) {
        m.diagnostics(db, &mut sink);
    };
    drop(sink);
    res.into_inner()
}

fn check_unnecessary_braces_in_use_statement(
    acc: &mut Vec<Diagnostic>,
    file_id: FileId,
    node: &SyntaxNode,
) -> Option<()> {
    let use_tree_list = ast::UseTreeList::cast(node.clone())?;
    if let Some((single_use_tree,)) = use_tree_list.use_trees().collect_tuple() {
        let range = use_tree_list.syntax().text_range();
        let edit =
            text_edit_for_remove_unnecessary_braces_with_self_in_use_statement(&single_use_tree)
                .unwrap_or_else(|| {
                    let to_replace = single_use_tree.syntax().text().to_string();
                    let mut edit_builder = TextEditBuilder::default();
                    edit_builder.delete(range);
                    edit_builder.insert(range.start(), to_replace);
                    edit_builder.finish()
                });

        acc.push(Diagnostic {
            range,
            message: "Unnecessary braces in use statement".to_string(),
            severity: Severity::WeakWarning,
            fix: Some(SourceChange::source_file_edit(
                "Remove unnecessary braces",
                SourceFileEdit { file_id, edit },
            )),
        });
    }

    Some(())
}

fn text_edit_for_remove_unnecessary_braces_with_self_in_use_statement(
    single_use_tree: &ast::UseTree,
) -> Option<TextEdit> {
    let use_tree_list_node = single_use_tree.syntax().parent()?;
    if single_use_tree.path()?.segment()?.syntax().first_child_or_token()?.kind() == T![self] {
        let start = use_tree_list_node.prev_sibling_or_token()?.text_range().start();
        let end = use_tree_list_node.text_range().end();
        let range = TextRange::new(start, end);
        return Some(TextEdit::delete(range));
    }
    None
}

fn check_struct_shorthand_initialization(
    acc: &mut Vec<Diagnostic>,
    file_id: FileId,
    node: &SyntaxNode,
) -> Option<()> {
    let record_lit = ast::RecordLit::cast(node.clone())?;
    let record_field_list = record_lit.record_field_list()?;
    for record_field in record_field_list.fields() {
        if let (Some(name_ref), Some(expr)) = (record_field.name_ref(), record_field.expr()) {
            let field_name = name_ref.syntax().text().to_string();
            let field_expr = expr.syntax().text().to_string();
            if field_name == field_expr {
                let mut edit_builder = TextEditBuilder::default();
                edit_builder.delete(record_field.syntax().text_range());
                edit_builder.insert(record_field.syntax().text_range().start(), field_name);
                let edit = edit_builder.finish();

                acc.push(Diagnostic {
                    range: record_field.syntax().text_range(),
                    message: "Shorthand struct initialization".to_string(),
                    severity: Severity::WeakWarning,
                    fix: Some(SourceChange::source_file_edit(
                        "Use struct shorthand initialization",
                        SourceFileEdit { file_id, edit },
                    )),
                });
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;
    use ra_syntax::SourceFile;
    use stdx::SepBy;
    use test_utils::assert_eq_text;

    use crate::mock_analysis::{analysis_and_position, single_file};

    use super::*;

    type DiagnosticChecker = fn(&mut Vec<Diagnostic>, FileId, &SyntaxNode) -> Option<()>;

    fn check_not_applicable(code: &str, func: DiagnosticChecker) {
        let parse = SourceFile::parse(code);
        let mut diagnostics = Vec::new();
        for node in parse.tree().syntax().descendants() {
            func(&mut diagnostics, FileId(0), &node);
        }
        assert!(diagnostics.is_empty());
    }

    fn check_apply(before: &str, after: &str, func: DiagnosticChecker) {
        let parse = SourceFile::parse(before);
        let mut diagnostics = Vec::new();
        for node in parse.tree().syntax().descendants() {
            func(&mut diagnostics, FileId(0), &node);
        }
        let diagnostic =
            diagnostics.pop().unwrap_or_else(|| panic!("no diagnostics for:\n{}\n", before));
        let mut fix = diagnostic.fix.unwrap();
        let edit = fix.source_file_edits.pop().unwrap().edit;
        let actual = edit.apply(&before);
        assert_eq_text!(after, &actual);
    }

    /// Takes a multi-file input fixture with annotated cursor positions,
    /// and checks that:
    ///  * a diagnostic is produced
    ///  * this diagnostic touches the input cursor position
    ///  * that the contents of the file containing the cursor match `after` after the diagnostic fix is applied
    fn check_apply_diagnostic_fix_from_position(fixture: &str, after: &str) {
        let (analysis, file_position) = analysis_and_position(fixture);
        let diagnostic = analysis.diagnostics(file_position.file_id).unwrap().pop().unwrap();
        let mut fix = diagnostic.fix.unwrap();
        let edit = fix.source_file_edits.pop().unwrap().edit;
        let target_file_contents = analysis.file_text(file_position.file_id).unwrap();
        let actual = edit.apply(&target_file_contents);

        // Strip indent and empty lines from `after`, to match the behaviour of
        // `parse_fixture` called from `analysis_and_position`.
        let margin = fixture
            .lines()
            .filter(|it| it.trim_start().starts_with("//-"))
            .map(|it| it.len() - it.trim_start().len())
            .next()
            .expect("empty fixture");
        let after = after
            .lines()
            .filter_map(|line| if line.len() > margin { Some(&line[margin..]) } else { None })
            .sep_by("\n")
            .suffix("\n")
            .to_string();

        assert_eq_text!(&after, &actual);
        assert!(
            diagnostic.range.start() <= file_position.offset
                && diagnostic.range.end() >= file_position.offset,
            "diagnostic range {:?} does not touch cursor position {:?}",
            diagnostic.range,
            file_position.offset
        );
    }

    fn check_apply_diagnostic_fix(before: &str, after: &str) {
        let (analysis, file_id) = single_file(before);
        let diagnostic = analysis.diagnostics(file_id).unwrap().pop().unwrap();
        let mut fix = diagnostic.fix.unwrap();
        let edit = fix.source_file_edits.pop().unwrap().edit;
        let actual = edit.apply(&before);
        assert_eq_text!(after, &actual);
    }

    /// Takes a multi-file input fixture with annotated cursor position and checks that no diagnostics
    /// apply to the file containing the cursor.
    fn check_no_diagnostic_for_target_file(fixture: &str) {
        let (analysis, file_position) = analysis_and_position(fixture);
        let diagnostics = analysis.diagnostics(file_position.file_id).unwrap();
        assert_eq!(diagnostics.len(), 0);
    }

    fn check_no_diagnostic(content: &str) {
        let (analysis, file_id) = single_file(content);
        let diagnostics = analysis.diagnostics(file_id).unwrap();
        assert_eq!(diagnostics.len(), 0, "expected no diagnostic, found one");
    }

    #[test]
    fn test_wrap_return_type() {
        let before = r#"
            //- /main.rs
            use std::{string::String, result::Result::{self, Ok, Err}};

            fn div(x: i32, y: i32) -> Result<i32, String> {
                if y == 0 {
                    return Err("div by zero".into());
                }
                x / y<|>
            }

            //- /std/lib.rs
            pub mod string {
                pub struct String { }
            }
            pub mod result {
                pub enum Result<T, E> { Ok(T), Err(E) }
            }
        "#;
        let after = r#"
            use std::{string::String, result::Result::{self, Ok, Err}};

            fn div(x: i32, y: i32) -> Result<i32, String> {
                if y == 0 {
                    return Err("div by zero".into());
                }
                Ok(x / y)
            }
        "#;
        check_apply_diagnostic_fix_from_position(before, after);
    }

    #[test]
    fn test_wrap_return_type_handles_generic_functions() {
        let before = r#"
            //- /main.rs
            use std::result::Result::{self, Ok, Err};

            fn div<T>(x: T) -> Result<T, i32> {
                if x == 0 {
                    return Err(7);
                }
                <|>x
            }

            //- /std/lib.rs
            pub mod result {
                pub enum Result<T, E> { Ok(T), Err(E) }
            }
        "#;
        let after = r#"
            use std::result::Result::{self, Ok, Err};

            fn div<T>(x: T) -> Result<T, i32> {
                if x == 0 {
                    return Err(7);
                }
                Ok(x)
            }
        "#;
        check_apply_diagnostic_fix_from_position(before, after);
    }

    #[test]
    fn test_wrap_return_type_handles_type_aliases() {
        let before = r#"
            //- /main.rs
            use std::{string::String, result::Result::{self, Ok, Err}};

            type MyResult<T> = Result<T, String>;

            fn div(x: i32, y: i32) -> MyResult<i32> {
                if y == 0 {
                    return Err("div by zero".into());
                }
                x <|>/ y
            }

            //- /std/lib.rs
            pub mod string {
                pub struct String { }
            }
            pub mod result {
                pub enum Result<T, E> { Ok(T), Err(E) }
            }
        "#;
        let after = r#"
            use std::{string::String, result::Result::{self, Ok, Err}};

            type MyResult<T> = Result<T, String>;
            fn div(x: i32, y: i32) -> MyResult<i32> {
                if y == 0 {
                    return Err("div by zero".into());
                }
                Ok(x / y)
            }
        "#;
        check_apply_diagnostic_fix_from_position(before, after);
    }

    #[test]
    fn test_wrap_return_type_not_applicable_when_expr_type_does_not_match_ok_type() {
        let content = r#"
            //- /main.rs
            use std::{string::String, result::Result::{self, Ok, Err}};

            fn foo() -> Result<String, i32> {
                0<|>
            }

            //- /std/lib.rs
            pub mod string {
                pub struct String { }
            }
            pub mod result {
                pub enum Result<T, E> { Ok(T), Err(E) }
            }
        "#;
        check_no_diagnostic_for_target_file(content);
    }

    #[test]
    fn test_wrap_return_type_not_applicable_when_return_type_is_not_result() {
        let content = r#"
            //- /main.rs
            use std::{string::String, result::Result::{self, Ok, Err}};

            enum SomeOtherEnum {
                Ok(i32),
                Err(String),
            }

            fn foo() -> SomeOtherEnum {
                0<|>
            }

            //- /std/lib.rs
            pub mod string {
                pub struct String { }
            }
            pub mod result {
                pub enum Result<T, E> { Ok(T), Err(E) }
            }
        "#;
        check_no_diagnostic_for_target_file(content);
    }

    #[test]
    fn test_fill_struct_fields_empty() {
        let before = r"
            struct TestStruct {
                one: i32,
                two: i64,
            }

            fn test_fn() {
                let s = TestStruct{};
            }
        ";
        let after = r"
            struct TestStruct {
                one: i32,
                two: i64,
            }

            fn test_fn() {
                let s = TestStruct{ one: (), two: ()};
            }
        ";
        check_apply_diagnostic_fix(before, after);
    }

    #[test]
    fn test_fill_struct_fields_self() {
        let before = r"
            struct TestStruct {
                one: i32,
            }

            impl TestStruct {
                fn test_fn() {
                    let s = Self {};
                }
            }
        ";
        let after = r"
            struct TestStruct {
                one: i32,
            }

            impl TestStruct {
                fn test_fn() {
                    let s = Self { one: ()};
                }
            }
        ";
        check_apply_diagnostic_fix(before, after);
    }

    #[test]
    fn test_fill_struct_fields_enum() {
        let before = r"
            enum Expr {
                Bin { lhs: Box<Expr>, rhs: Box<Expr> }
            }

            impl Expr {
                fn new_bin(lhs: Box<Expr>, rhs: Box<Expr>) -> Expr {
                    Expr::Bin { <|> }
                }
            }

        ";
        let after = r"
            enum Expr {
                Bin { lhs: Box<Expr>, rhs: Box<Expr> }
            }

            impl Expr {
                fn new_bin(lhs: Box<Expr>, rhs: Box<Expr>) -> Expr {
                    Expr::Bin { lhs: (), rhs: () <|> }
                }
            }

        ";
        check_apply_diagnostic_fix(before, after);
    }

    #[test]
    fn test_fill_struct_fields_partial() {
        let before = r"
            struct TestStruct {
                one: i32,
                two: i64,
            }

            fn test_fn() {
                let s = TestStruct{ two: 2 };
            }
        ";
        let after = r"
            struct TestStruct {
                one: i32,
                two: i64,
            }

            fn test_fn() {
                let s = TestStruct{ two: 2, one: () };
            }
        ";
        check_apply_diagnostic_fix(before, after);
    }

    #[test]
    fn test_fill_struct_fields_no_diagnostic() {
        let content = r"
            struct TestStruct {
                one: i32,
                two: i64,
            }

            fn test_fn() {
                let one = 1;
                let s = TestStruct{ one, two: 2 };
            }
        ";

        check_no_diagnostic(content);
    }

    #[test]
    fn test_fill_struct_fields_no_diagnostic_on_spread() {
        let content = r"
            struct TestStruct {
                one: i32,
                two: i64,
            }

            fn test_fn() {
                let one = 1;
                let s = TestStruct{ ..a };
            }
        ";

        check_no_diagnostic(content);
    }

    #[test]
    fn test_unresolved_module_diagnostic() {
        let (analysis, file_id) = single_file("mod foo;");
        let diagnostics = analysis.diagnostics(file_id).unwrap();
        assert_debug_snapshot!(diagnostics, @r###"
        [
            Diagnostic {
                message: "unresolved module",
                range: 0..8,
                fix: Some(
                    SourceChange {
                        label: "Create module",
                        source_file_edits: [],
                        file_system_edits: [
                            CreateFile {
                                source_root: SourceRootId(
                                    0,
                                ),
                                path: "foo.rs",
                            },
                        ],
                        cursor_position: None,
                    },
                ),
                severity: Error,
            },
        ]
        "###);
    }

    #[test]
    fn range_mapping_out_of_macros() {
        let (analysis, file_id) = single_file(
            r"
            fn some() {}
            fn items() {}
            fn here() {}

            macro_rules! id {
                ($($tt:tt)*) => { $($tt)*};
            }

            fn main() {
                let _x = id![Foo { a: 42 }];
            }

            pub struct Foo {
                pub a: i32,
                pub b: i32,
            }
        ",
        );
        let diagnostics = analysis.diagnostics(file_id).unwrap();
        assert_debug_snapshot!(diagnostics, @r###"
        [
            Diagnostic {
                message: "Missing structure fields:\n- b",
                range: 224..233,
                fix: Some(
                    SourceChange {
                        label: "Fill struct fields",
                        source_file_edits: [
                            SourceFileEdit {
                                file_id: FileId(
                                    1,
                                ),
                                edit: TextEdit {
                                    atoms: [
                                        AtomTextEdit {
                                            delete: 3..9,
                                            insert: "{a:42, b: ()}",
                                        },
                                    ],
                                },
                            },
                        ],
                        file_system_edits: [],
                        cursor_position: None,
                    },
                ),
                severity: Error,
            },
        ]
        "###);
    }

    #[test]
    fn test_check_unnecessary_braces_in_use_statement() {
        check_not_applicable(
            "
            use a;
            use a::{c, d::e};
        ",
            check_unnecessary_braces_in_use_statement,
        );
        check_apply("use {b};", "use b;", check_unnecessary_braces_in_use_statement);
        check_apply("use a::{c};", "use a::c;", check_unnecessary_braces_in_use_statement);
        check_apply("use a::{self};", "use a;", check_unnecessary_braces_in_use_statement);
        check_apply(
            "use a::{c, d::{e}};",
            "use a::{c, d::e};",
            check_unnecessary_braces_in_use_statement,
        );
    }

    #[test]
    fn test_check_struct_shorthand_initialization() {
        check_not_applicable(
            r#"
            struct A {
                a: &'static str
            }

            fn main() {
                A {
                    a: "hello"
                }
            }
        "#,
            check_struct_shorthand_initialization,
        );

        check_apply(
            r#"
struct A {
    a: &'static str
}

fn main() {
    let a = "haha";
    A {
        a: a
    }
}
        "#,
            r#"
struct A {
    a: &'static str
}

fn main() {
    let a = "haha";
    A {
        a
    }
}
        "#,
            check_struct_shorthand_initialization,
        );

        check_apply(
            r#"
struct A {
    a: &'static str,
    b: &'static str
}

fn main() {
    let a = "haha";
    let b = "bb";
    A {
        a: a,
        b
    }
}
        "#,
            r#"
struct A {
    a: &'static str,
    b: &'static str
}

fn main() {
    let a = "haha";
    let b = "bb";
    A {
        a,
        b
    }
}
        "#,
            check_struct_shorthand_initialization,
        );
    }
}
