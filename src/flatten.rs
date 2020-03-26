//! Tools for converting a [`cddl::ast`] (syntax tree) into an [`ivt`].
//!
//! This module is called "flatten" because its goal is to flatten syntax
//! tree detail that's not useful for validation.
//!
//! For example, this CDDL description:
//! ```text
//! name_type = tstr
//! name_group = (name: name_type)
//! object = { name_group }
//! ```
//! is functionally identical to this "flattened" one:
//! ```text
//! object = { name: tstr }
//! ```
//!

use crate::ivt::*;
use crate::util::ValidateError;
use cddl::ast::{self, CDDL};
use cddl::parser::cddl_from_str;
use std::collections::BTreeMap;
use std::sync::Arc;

pub type FlattenResult<T> = std::result::Result<T, ValidateError>;

pub type MutateResult = std::result::Result<(), ValidateError>;

pub type RulesByName = BTreeMap<String, ArcNode>;

pub fn flatten_from_str(cddl_input: &str) -> FlattenResult<RulesByName> {
    let cddl = cddl_from_str(cddl_input).map_err(|e| {
        // FIXME: don't throw away the original error
        let msg = format!("cddl parse error {}", e);
        ValidateError::Oops(msg)
    })?;
    flatten(&cddl)
}

pub fn flatten(ast: &CDDL) -> FlattenResult<RulesByName> {
    // This first pass generates a tree of Nodes from the AST.
    let rules: RulesByName = ast.rules.iter().map(|rule| flatten_rule(rule)).collect();
    // This second pass adds Weak references for by-name rule references.
    replace_rule_refs(&rules)?;
    Ok(rules)
}

// Descend recursively into a tree of Nodes, running a function against each.
fn mutate_node_tree<F>(node: &Node, func: &mut F) -> MutateResult
where
    F: FnMut(&Node) -> MutateResult,
{
    // Apply the function first, then recurse.
    func(node)?;
    match node {
        Node::Literal(_) => (),     // leaf node
        Node::PreludeType(_) => (), // leaf node
        Node::Rule(_) => (),        // leaf node
        Node::Choice(c) => {
            for option in &c.options {
                mutate_node_tree(option.as_ref(), func)?;
            }
        }
        Node::Map(m) => {
            for kv in &m.members {
                mutate_node_tree(kv.key.as_ref(), func)?;
                mutate_node_tree(kv.value.as_ref(), func)?;
            }
        }
        //Node::ArrayRecord(a) => ___,
        //Node::ArrayVec(a) => ___,
        _ => unimplemented!(),
    }
    Ok(())
}

fn replace_rule_refs(rules: &RulesByName) -> MutateResult {
    for root in rules.values() {
        mutate_node_tree(root, &mut |node| {
            match node {
                Node::Rule(rule_ref) => {
                    // FIXME: add graceful handling of nonexistent rule name
                    let real_ref = rules.get(&rule_ref.name);
                    if real_ref.is_none() {
                        panic!("tried to access nonexistent rule '{}'", &rule_ref.name);
                    }
                    let real_ref = real_ref.unwrap();
                    rule_ref.upgrade(real_ref);
                }
                _ => (),
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn flatten_rule(rule: &ast::Rule) -> (String, ArcNode) {
    let (name, node) = match rule {
        ast::Rule::Type { rule, .. } => flatten_typerule(rule),
        _ => unimplemented!(),
    };
    (name, Arc::new(node))
}

fn flatten_typerule(typerule: &ast::TypeRule) -> (String, Node) {
    // FIXME: handle generic_param
    // FIXME: handle is_type_choice_alternate
    let rhs = flatten_type(&typerule.value);
    (typerule.name.ident.clone(), rhs)
}

fn flatten_type(ty: &ast::Type) -> Node {
    // FIXME: len > 1 means we should emit a Choice instead.
    assert!(ty.type_choices.len() == 1);
    let ty1 = &ty.type_choices[0];
    flatten_type1(ty1)
}

fn flatten_type1(ty1: &ast::Type1) -> Node {
    // FIXME: handle range & control operators.
    flatten_type2(&ty1.type2)
}

fn flatten_type2(ty2: &ast::Type2) -> Node {
    use ast::Type2;
    match ty2 {
        // FIXME: this casting is gross.
        Type2::UintValue { value, .. } => Node::Literal(Literal::Int(*value as i128)),
        Type2::TextValue { value, .. } => Node::Literal(Literal::Text(value.clone())),
        Type2::Typename { ident, .. } => flatten_typename(&ident.ident),
        Type2::Map { group, .. } => flatten_map(&group),
        _ => unimplemented!(),
    }
}

fn flatten_typename(name: &str) -> Node {
    match name {
        "any" => Node::PreludeType(PreludeType::Any),
        "bool" => Node::PreludeType(PreludeType::Bool),
        "false" => Node::Literal(Literal::Bool(false)),
        "true" => Node::Literal(Literal::Bool(true)),
        "int" => Node::PreludeType(PreludeType::Int),
        "uint" => Node::PreludeType(PreludeType::Uint),
        "tstr" => Node::PreludeType(PreludeType::Tstr),
        // FIXME: lots more prelude types to handle...
        // FIXME: this could be a group name, maybe other things?
        _ => Node::Rule(Rule::new(name)),
    }
}

fn flatten_map(group: &ast::Group) -> Node {
    // FIXME: len > 1 means we should emit a Choice instead.
    assert!(group.group_choices.len() == 1);
    let grpchoice = &group.group_choices[0];
    let nodes: Vec<KeyValue> = grpchoice
        .group_entries
        .iter()
        .map(|ge_tuple| {
            let group_entry = &ge_tuple.0;
            flatten_groupentry(group_entry)
        })
        .collect();
    Node::Map(Map { members: nodes })
}

fn flatten_groupentry(group_entry: &ast::GroupEntry) -> KeyValue {
    use ast::GroupEntry;
    // FIXME: does this need different behavior for maps vs arrays(record or vector)?
    match group_entry {
        GroupEntry::ValueMemberKey { ge, .. } => flatten_vmke(ge),
        GroupEntry::TypeGroupname { .. } => unimplemented!(),
        GroupEntry::InlineGroup { .. } => unimplemented!(),
    }
}

// FIXME: this was a fun idea, but the implementation is kind of annoying.
// I think I'd rather go back to the AST-style enum instead of this
// confusing numeric system.
impl From<&Option<ast::Occur>> for Occur {
    fn from(occur: &Option<ast::Occur>) -> Occur {
        match occur {
            None => Occur { lower: 1, upper: 1 },
            Some(ast::Occur::Optional(_)) => Occur { lower: 0, upper: 1 },
            Some(ast::Occur::ZeroOrMore(_)) => Occur {
                lower: 0,
                upper: usize::MAX,
            },
            Some(ast::Occur::OneOrMore(_)) => Occur {
                lower: 1,
                upper: usize::MAX,
            },
            Some(ast::Occur::Exact { lower, upper, .. }) => {
                let lower = lower.unwrap_or(0);
                let upper = upper.unwrap_or(usize::MAX);
                Occur { lower, upper }
            }
        }
    }
}

fn flatten_vmke(vmke: &ast::ValueMemberKeyEntry) -> KeyValue {
    let occur = Occur::from(&vmke.occur);
    let member_key = vmke.member_key.as_ref().unwrap(); // FIXME: may be None for arrays
    let key = flatten_memberkey(&member_key);
    let value = flatten_type(&vmke.entry_type);
    KeyValue::new(key, value, occur)
}

fn flatten_memberkey(memberkey: &ast::MemberKey) -> Node {
    use ast::MemberKey;
    match memberkey {
        MemberKey::Bareword { ident, .. } => {
            // A "bareword" is just a literal string used in the context
            // of a map key.
            let name = ident.ident.clone();
            Node::Literal(Literal::Text(name))
        }
        // FIXME: handle cut
        MemberKey::Type1 { t1, .. } => flatten_type1(t1.as_ref()),
        _ => unimplemented!(),
    }
}

#[test]
fn test_flatten_literal_int() {
    let cddl_input = r#"thing = 1"#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    assert_eq!(result, r#"{"thing": Literal(Int(1))}"#);
}

#[test]
fn test_flatten_literal_tstr() {
    let cddl_input = r#"thing = "abc""#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    assert_eq!(result, r#"{"thing": Literal(Text("abc"))}"#);
}

#[test]
fn test_flatten_prelude_reference() {
    let cddl_input = r#"thing = int"#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    assert_eq!(result, r#"{"thing": PreludeType(Int)}"#);
}

#[test]
#[ignore] // FIXME: choking on dangling type reference
fn test_flatten_type_reference() {
    let cddl_input = r#"thing = foo"#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    assert_eq!(result, r#"{"thing": Rule(Rule { name: "foo!" })}"#);
}

#[test]
fn test_flatten_map() {
    // A map containing a bareword key
    let cddl_input = r#"thing = { foo: tstr }"#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    let expected = concat!(
        r#"{"thing": Map(Map { members: [KeyValue(Literal(Text("foo")), PreludeType(Tstr))] })}"#
    );
    assert_eq!(result, expected);

    // A map containing a prelude type key.
    // Note: CDDL syntax requires type keys to use "=>" not ":", otherwise
    // it will assume a bareword key is being used.
    let cddl_input = r#"thing = { tstr => tstr }"#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    let expected = concat!(
        r#"{"thing": Map(Map { members: [KeyValue(PreludeType(Tstr), PreludeType(Tstr))] })}"#
    );
    assert_eq!(result, expected);

    // A map key name alias
    let cddl_input = r#"foo = "bar" thing = { foo => tstr }"#;
    let result = flatten_from_str(cddl_input).unwrap();
    let result = format!("{:?}", result);
    let expected = concat!(
        r#"{"foo": Literal(Text("bar")), "thing": Map(Map { members: ["#,
        r#"KeyValue(Rule(Rule { name: "foo!" }), PreludeType(Tstr))] })}"#
    );
    // FIXME: is Rule the right output?  What if "abc" was a group name?
    assert_eq!(result, expected);
}
