//! Crate-qualified names for types that share a bare name across crates.

use tpt_moira_clotho::ast::{ItemKind, Program, TypeKind};
use tpt_moira_clotho::resolve::{disambiguate, NamedCrate};

fn krate(key: &str, is_root: bool, src: &str) -> NamedCrate {
    NamedCrate {
        key: key.to_string(),
        is_root,
        program: tpt_moira_clotho::parse_source(src).expect("should parse"),
    }
}

fn struct_names(p: &Program) -> Vec<String> {
    p.items
        .iter()
        .filter_map(|i| match &i.kind {
            ItemKind::Struct(s) => Some(s.name.node.clone()),
            _ => None,
        })
        .collect()
}

/// The path names of the first field's type of the struct `name`.
fn field_type_path(p: &Program, name: &str) -> Vec<String> {
    for item in &p.items {
        if let ItemKind::Struct(s) = &item.kind {
            if s.name.node == name {
                if let TypeKind::Path(path, _) = &s.fields[0].ty.kind {
                    return path.segments.iter().map(|x| x.node.clone()).collect();
                }
            }
        }
    }
    panic!("no struct {name}");
}

#[test]
fn extern_definition_is_renamed_and_root_keeps_its_name() {
    let root_src = "use tpt_moira_lib::Function;\n\
                    pub struct Function { pub name: i64 }\n\
                    pub struct Holder { pub f: Function }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let mut crates = vec![krate("", true, root_src), krate("lib", false, lib_src)];
    disambiguate(&mut crates);
    assert_eq!(struct_names(&crates[0].program), vec!["Function", "Holder"]);
    assert_eq!(struct_names(&crates[1].program), vec!["lib_Function"]);
}

#[test]
fn qualified_references_to_the_extern_type_are_rewritten() {
    // The root names the extern's type through a module alias (`lib::Function`).
    let root_src = "use tpt_moira_lib::lib;\n\
                    pub struct Function { pub n: i64 }\n\
                    pub struct Holder { pub f: lib::Function }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let mut crates = vec![krate("", true, root_src), krate("lib", false, lib_src)];
    disambiguate(&mut crates);
    assert_eq!(
        field_type_path(&crates[0].program, "Holder"),
        vec!["lib", "lib_Function"]
    );
}

#[test]
fn names_without_a_collision_are_untouched() {
    let root_src = "pub struct Holder { pub n: i64 }\n";
    let lib_src = "pub struct Other { pub s: String }\n";
    let mut crates = vec![krate("", true, root_src), krate("lib", false, lib_src)];
    disambiguate(&mut crates);
    assert_eq!(struct_names(&crates[0].program), vec!["Holder"]);
    assert_eq!(struct_names(&crates[1].program), vec!["Other"]);
}

#[test]
fn glob_imported_extern_type_is_rewritten_in_the_root() {
    // The root names the extern's type through a glob import, as `use lib::ast::*` does.
    let root_src = "use tpt_moira_lib::*;\n\
                    pub struct Holder { pub f: Function }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let other_src = "pub struct Function { pub n: i64 }\n";
    let mut crates = vec![
        krate("", true, root_src),
        krate("lib", false, lib_src),
        krate("other", false, other_src),
    ];
    disambiguate(&mut crates);
    assert_eq!(
        field_type_path(&crates[0].program, "Holder"),
        vec!["lib_Function"]
    );
}

#[test]
fn glob_of_a_module_path_renames_a_reference_parameter_type() {
    // `use tpt_moira_lib::ast::*;` then `f: &mut Function`, the shape the compiler's own
    // sources use.
    let root_src = "use tpt_moira_lib::ast::*;\n\
                    pub fn touch(f: &mut Function) -> i64 { 0 }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let other_src = "pub struct Function { pub n: i64 }\n";
    let mut crates = vec![
        krate("", true, root_src),
        krate("lib", false, lib_src),
        krate("other", false, other_src),
    ];
    disambiguate(&mut crates);
    let f = crates[0]
        .program
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ItemKind::Function(f) if f.name.node == "touch" => Some(f),
            _ => None,
        })
        .expect("the function");
    let TypeKind::RefMut(inner) = &f.params[0].ty.kind else {
        panic!("expected a &mut parameter");
    };
    let TypeKind::Path(p, _) = &inner.kind else {
        panic!("expected a path type");
    };
    assert_eq!(p.segments[0].node, "lib_Function");
}

#[test]
fn two_glob_imports_of_one_crate_still_resolve_the_name() {
    // Two modules of one crate each glob-import the same extern module.
    let root_src = "use tpt_moira_lib::ast::*;\n\
                    use tpt_moira_lib::ast::*;\n\
                    pub fn touch(f: &mut Function) -> i64 { 0 }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let other_src = "pub struct Function { pub n: i64 }\n";
    let mut crates = vec![
        krate("", true, root_src),
        krate("lib", false, lib_src),
        krate("other", false, other_src),
    ];
    disambiguate(&mut crates);
    let f = crates[0]
        .program
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ItemKind::Function(f) if f.name.node == "touch" => Some(f),
            _ => None,
        })
        .expect("the function");
    let TypeKind::RefMut(inner) = &f.params[0].ty.kind else {
        panic!("expected a &mut parameter");
    };
    let TypeKind::Path(p, _) = &inner.kind else {
        panic!("expected a path type");
    };
    assert_eq!(p.segments[0].node, "lib_Function");
}

#[test]
fn aliased_import_of_a_colliding_type_is_renamed() {
    let root_src = "use tpt_moira_lib::Function as LibFn;\n\
                    pub struct Function { pub n: i64 }\n\
                    pub struct Holder { pub f: LibFn }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let mut crates = vec![krate("", true, root_src), krate("lib", false, lib_src)];
    disambiguate(&mut crates);
    let root = &crates[0].program;
    assert_eq!(field_type_path(root, "Holder"), vec!["lib_Function".to_string()]);
}

#[test]
fn extern_key_may_carry_the_tpt_moira_prefix() {
    let root_src = "use tpt_moira_lib::Function;\n\
                    pub struct Holder { pub f: Function }\n";
    let lib_src = "pub struct Function { pub name: String }\n";
    let other_src = "pub struct Function { pub n: i64 }\n";
    let mut crates = vec![
        krate("", true, root_src),
        krate("tpt_moira_lib", false, lib_src),
        krate("other", false, other_src),
    ];
    disambiguate(&mut crates);
    let root = &crates[0].program;
    assert_eq!(field_type_path(root, "Holder"), vec!["tpt_moira_lib_Function".to_string()]);
}
