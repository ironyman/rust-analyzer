use ra_db::{FileId, SourceDatabase};
use ra_syntax::{
    AstNode, ast::{self, DocCommentsOwner},
    algo::{
        find_node_at_offset,
        visit::{visitor, Visitor},
    },
    SyntaxNode,
};

use crate::{
    FilePosition, NavigationTarget,
    db::RootDatabase,
    RangeInfo,
    name_ref_kind::{NameRefKind::*, classify_name_ref},
    display::ShortLabel,
};

pub(crate) fn goto_definition(
    db: &RootDatabase,
    position: FilePosition,
) -> Option<RangeInfo<Vec<NavigationTarget>>> {
    let file = db.parse(position.file_id).tree;
    let syntax = file.syntax();
    if let Some(name_ref) = find_node_at_offset::<ast::NameRef>(syntax, position.offset) {
        let navs = reference_definition(db, position.file_id, name_ref).to_vec();
        return Some(RangeInfo::new(name_ref.syntax().range(), navs.to_vec()));
    }
    if let Some(name) = find_node_at_offset::<ast::Name>(syntax, position.offset) {
        let navs = name_definition(db, position.file_id, name)?;
        return Some(RangeInfo::new(name.syntax().range(), navs));
    }
    None
}

#[derive(Debug)]
pub(crate) enum ReferenceResult {
    Exact(NavigationTarget),
    Approximate(Vec<NavigationTarget>),
}

impl ReferenceResult {
    fn to_vec(self) -> Vec<NavigationTarget> {
        use self::ReferenceResult::*;
        match self {
            Exact(target) => vec![target],
            Approximate(vec) => vec,
        }
    }
}

pub(crate) fn reference_definition(
    db: &RootDatabase,
    file_id: FileId,
    name_ref: &ast::NameRef,
) -> ReferenceResult {
    use self::ReferenceResult::*;

    let analyzer = hir::SourceAnalyzer::new(db, file_id, name_ref.syntax(), None);

    match classify_name_ref(db, &analyzer, name_ref) {
        Some(Macro(mac)) => return Exact(NavigationTarget::from_macro_def(db, mac)),
        Some(FieldAccess(field)) => return Exact(NavigationTarget::from_field(db, field)),
        Some(AssocItem(assoc)) => return Exact(NavigationTarget::from_impl_item(db, assoc)),
        Some(Method(func)) => return Exact(NavigationTarget::from_def_source(db, func)),
        Some(Def(def)) => match NavigationTarget::from_def(db, def) {
            Some(nav) => return Exact(nav),
            None => return Approximate(vec![]),
        },
        Some(SelfType(ty)) => {
            if let Some((def_id, _)) = ty.as_adt() {
                return Exact(NavigationTarget::from_adt_def(db, def_id));
            }
        }
        Some(Pat(pat)) => return Exact(NavigationTarget::from_pat(db, file_id, pat)),
        Some(SelfParam(par)) => return Exact(NavigationTarget::from_self_param(file_id, par)),
        Some(GenericParam(_)) => {
            // FIXME: go to the generic param def
        }
        None => {}
    };

    // Fallback index based approach:
    let navs = crate::symbol_index::index_resolve(db, name_ref)
        .into_iter()
        .map(|s| NavigationTarget::from_symbol(db, s))
        .collect();
    Approximate(navs)
}

pub(crate) fn name_definition(
    db: &RootDatabase,
    file_id: FileId,
    name: &ast::Name,
) -> Option<Vec<NavigationTarget>> {
    let parent = name.syntax().parent()?;

    if let Some(module) = ast::Module::cast(&parent) {
        if module.has_semi() {
            if let Some(child_module) =
                hir::source_binder::module_from_declaration(db, file_id, module)
            {
                let nav = NavigationTarget::from_module(db, child_module);
                return Some(vec![nav]);
            }
        }
    }

    if let Some(nav) = named_target(file_id, &parent) {
        return Some(vec![nav]);
    }

    None
}

fn named_target(file_id: FileId, node: &SyntaxNode) -> Option<NavigationTarget> {
    visitor()
        .visit(|node: &ast::StructDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::EnumDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::EnumVariant| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::FnDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::TypeAliasDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::ConstDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::StaticDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::TraitDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::NamedFieldDef| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::Module| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), node.short_label())
        })
        .visit(|node: &ast::MacroCall| {
            NavigationTarget::from_named(file_id, node, node.doc_comment_text(), None)
        })
        .accept(node)
}

#[cfg(test)]
mod tests {
    use test_utils::covers;

    use crate::mock_analysis::analysis_and_position;

    fn check_goto(fixture: &str, expected: &str) {
        let (analysis, pos) = analysis_and_position(fixture);

        let mut navs = analysis.goto_definition(pos).unwrap().unwrap().info;
        assert_eq!(navs.len(), 1);
        let nav = navs.pop().unwrap();
        nav.assert_match(expected);
    }

    #[test]
    fn goto_definition_works_in_items() {
        check_goto(
            "
            //- /lib.rs
            struct Foo;
            enum E { X(Foo<|>) }
            ",
            "Foo STRUCT_DEF FileId(1) [0; 11) [7; 10)",
        );
    }

    #[test]
    fn goto_definition_resolves_correct_name() {
        check_goto(
            "
            //- /lib.rs
            use a::Foo;
            mod a;
            mod b;
            enum E { X(Foo<|>) }
            //- /a.rs
            struct Foo;
            //- /b.rs
            struct Foo;
            ",
            "Foo STRUCT_DEF FileId(2) [0; 11) [7; 10)",
        );
    }

    #[test]
    fn goto_definition_works_for_module_declaration() {
        check_goto(
            "
            //- /lib.rs
            mod <|>foo;
            //- /foo.rs
            // empty
            ",
            "foo SOURCE_FILE FileId(2) [0; 10)",
        );

        check_goto(
            "
            //- /lib.rs
            mod <|>foo;
            //- /foo/mod.rs
            // empty
            ",
            "foo SOURCE_FILE FileId(2) [0; 10)",
        );
    }

    #[test]
    fn goto_definition_works_for_macros() {
        covers!(goto_definition_works_for_macros);
        check_goto(
            "
            //- /lib.rs
            macro_rules! foo {
                () => {
                    {}
                };
            }

            fn bar() {
                <|>foo!();
            }
            ",
            "foo MACRO_CALL FileId(1) [0; 50) [13; 16)",
        );
    }

    #[test]
    fn goto_definition_works_for_macros_from_other_crates() {
        covers!(goto_definition_works_for_macros);
        check_goto(
            "
            //- /lib.rs
            use foo::foo;
            fn bar() {
                <|>foo!();
            }

            //- /foo/lib.rs
            #[macro_export]
            macro_rules! foo {
                () => {
                    {}
                };
            }
            ",
            "foo MACRO_CALL FileId(2) [0; 66) [29; 32)",
        );
    }

    #[test]
    fn goto_definition_works_for_methods() {
        covers!(goto_definition_works_for_methods);
        check_goto(
            "
            //- /lib.rs
            struct Foo;
            impl Foo {
                fn frobnicate(&self) {  }
            }

            fn bar(foo: &Foo) {
                foo.frobnicate<|>();
            }
            ",
            "frobnicate FN_DEF FileId(1) [27; 52) [30; 40)",
        );
    }

    #[test]
    fn goto_definition_works_for_fields() {
        covers!(goto_definition_works_for_fields);
        check_goto(
            "
            //- /lib.rs
            struct Foo {
                spam: u32,
            }

            fn bar(foo: &Foo) {
                foo.spam<|>;
            }
            ",
            "spam NAMED_FIELD_DEF FileId(1) [17; 26) [17; 21)",
        );
    }

    #[test]
    fn goto_definition_works_for_named_fields() {
        covers!(goto_definition_works_for_named_fields);
        check_goto(
            "
            //- /lib.rs
            struct Foo {
                spam: u32,
            }

            fn bar() -> Foo {
                Foo {
                    spam<|>: 0,
                }
            }
            ",
            "spam NAMED_FIELD_DEF FileId(1) [17; 26) [17; 21)",
        );
    }
    #[test]
    fn goto_definition_on_self() {
        check_goto(
            "
            //- /lib.rs
            struct Foo;
            impl Foo {
                pub fn new() -> Self {
                    Self<|> {}
                }
            }
            ",
            "Foo STRUCT_DEF FileId(1) [0; 11) [7; 10)",
        );

        check_goto(
            "
            //- /lib.rs
            struct Foo;
            impl Foo {
                pub fn new() -> Self<|> {
                    Self {}
                }
            }
            ",
            "Foo STRUCT_DEF FileId(1) [0; 11) [7; 10)",
        );

        check_goto(
            "
            //- /lib.rs
            enum Foo { A }
            impl Foo {
                pub fn new() -> Self<|> {
                    Foo::A
                }
            }
            ",
            "Foo ENUM_DEF FileId(1) [0; 14) [5; 8)",
        );

        check_goto(
            "
            //- /lib.rs
            enum Foo { A }
            impl Foo {
                pub fn thing(a: &Self<|>) {
                }
            }
            ",
            "Foo ENUM_DEF FileId(1) [0; 14) [5; 8)",
        );
    }

    #[test]
    fn goto_definition_on_self_in_trait_impl() {
        check_goto(
            "
            //- /lib.rs
            struct Foo;
            trait Make {
                fn new() -> Self;
            }
            impl Make for Foo {
                fn new() -> Self {
                    Self<|> {}
                }
            }
            ",
            "Foo STRUCT_DEF FileId(1) [0; 11) [7; 10)",
        );

        check_goto(
            "
            //- /lib.rs
            struct Foo;
            trait Make {
                fn new() -> Self;
            }
            impl Make for Foo {
                fn new() -> Self<|> {
                    Self{}
                }
            }
            ",
            "Foo STRUCT_DEF FileId(1) [0; 11) [7; 10)",
        );
    }

    #[test]
    fn goto_definition_works_when_used_on_definition_name_itself() {
        check_goto(
            "
            //- /lib.rs
            struct Foo<|> { value: u32 }
            ",
            "Foo STRUCT_DEF FileId(1) [0; 25) [7; 10)",
        );

        check_goto(
            r#"
            //- /lib.rs
            struct Foo {
                field<|>: string,
            }
            "#,
            "field NAMED_FIELD_DEF FileId(1) [17; 30) [17; 22)",
        );

        check_goto(
            "
            //- /lib.rs
            fn foo_test<|>() {
            }
            ",
            "foo_test FN_DEF FileId(1) [0; 17) [3; 11)",
        );

        check_goto(
            "
            //- /lib.rs
            enum Foo<|> {
                Variant,
            }
            ",
            "Foo ENUM_DEF FileId(1) [0; 25) [5; 8)",
        );

        check_goto(
            "
            //- /lib.rs
            enum Foo {
                Variant1,
                Variant2<|>,
                Variant3,
            }
            ",
            "Variant2 ENUM_VARIANT FileId(1) [29; 37) [29; 37)",
        );

        check_goto(
            r#"
            //- /lib.rs
            static inner<|>: &str = "";
            "#,
            "inner STATIC_DEF FileId(1) [0; 24) [7; 12)",
        );

        check_goto(
            r#"
            //- /lib.rs
            const inner<|>: &str = "";
            "#,
            "inner CONST_DEF FileId(1) [0; 23) [6; 11)",
        );

        check_goto(
            r#"
            //- /lib.rs
            type Thing<|> = Option<()>;
            "#,
            "Thing TYPE_ALIAS_DEF FileId(1) [0; 24) [5; 10)",
        );

        check_goto(
            r#"
            //- /lib.rs
            trait Foo<|> {
            }
            "#,
            "Foo TRAIT_DEF FileId(1) [0; 13) [6; 9)",
        );

        check_goto(
            r#"
            //- /lib.rs
            mod bar<|> {
            }
            "#,
            "bar MODULE FileId(1) [0; 11) [4; 7)",
        );
    }
}
