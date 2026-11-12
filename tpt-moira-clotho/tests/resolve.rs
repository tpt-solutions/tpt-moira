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
