//! Engine facts the content layer is written against.
//!
//! These are not tests of tuckin: they pin the third-party behavior that the content layer is
//! defined in terms of — how a backslash run before `{{` is read, how a tag alone on a line
//! moves bytes, which AST nodes exist, and where `which` looks — so that an engine upgrade that
//! changes one of them fails here instead of silently changing what `apply` writes.

use std::collections::HashMap;

use handlebars::template::{HelperTemplate, Parameter, TemplateElement};
use handlebars::{Context, Handlebars, RenderContext, Renderable, Template};

/// Renders with the settings the content layer will use: no HTML escaping, one compile, one
/// render. Data is a flat string map because the template surface is helpers with literal
/// arguments, not context lookups.
fn render(source: &str, data: &HashMap<&str, &str>) -> String {
    let mut registry = Handlebars::new();
    registry.register_escape_fn(handlebars::no_escape);
    let template = Template::compile(source).unwrap();
    let context = Context::wraps(data).unwrap();
    let mut render_context = RenderContext::new(None);
    template.renders(&registry, &context, &mut render_context).unwrap()
}

fn data(pairs: &[(&'static str, &'static str)]) -> HashMap<&'static str, &'static str> {
    pairs.iter().copied().collect()
}

#[test]
fn one_backslash_makes_the_open_tag_literal() {
    assert_eq!(render(r"a\{{x}}b", &data(&[("x", "V")])), "a{{x}}b");
}

#[test]
fn two_backslashes_are_one_backslash_and_a_real_tag() {
    assert_eq!(render(r"a\\{{x}}b", &data(&[("x", "V")])), r"a\Vb");
}

#[test]
fn three_backslashes_are_two_backslashes_and_a_real_tag() {
    assert_eq!(render(r"a\\\{{x}}b", &data(&[("x", "V")])), r"a\\Vb");
}

#[test]
fn output_is_not_html_escaped() {
    let value = r#"<&"'>"#;
    assert_eq!(render("{{x}}", &data(&[("x", value)])), value);
}

/// An open and a close tag each alone on their line disappear with their line, and the body
/// keeps the newline that precedes the close tag. This is the byte boundary patch bodies are
/// defined by, so it is asserted as bytes rather than as a line list.
#[test]
fn block_tags_alone_on_their_lines_leave_the_body_newline() {
    let source = "a\n{{#if flag}}\nbody\n{{/if}}\nb\n";
    assert_eq!(render(source, &data(&[("flag", "yes")])), "a\nbody\nb\n");
}

#[test]
fn indentation_before_a_standalone_close_tag_is_not_body() {
    let source = "a\n{{#if flag}}\nbody\n  {{/if}}\nb\n";
    assert_eq!(render(source, &data(&[("flag", "yes")])), "a\nbody\nb\n");
}

/// A tag sharing a line with text is not standalone: neither surrounding newline moves.
#[test]
fn a_tag_sharing_a_line_moves_no_newline() {
    let source = "a\n{{#if flag}}x\n{{/if}}b\n";
    assert_eq!(render(source, &data(&[("flag", "yes")])), "a\nx\nb\n");
}

/// The AST names every tag kind the content layer must either accept or refuse, and it exposes
/// a `patch` block's arguments and body. Closure is decided from these nodes, not from the text.
///
/// A plain `{{name}}` and a helper call `{{helper "arg"}}` are the same element kind and are
/// told apart by their name node: a path is a value reference, a bare name is a helper.
#[test]
fn the_ast_exposes_every_tag_kind_and_patch_arguments() {
    let source = r#"{{x}}{{env "HOME"}}{{#patch at="A" to="B"}}body{{/patch}}{{! note}}{{> part}}{{* deco}}"#;
    let template = Template::compile(source).unwrap();
    let elements = &template.elements;
    assert_eq!(elements.len(), 6, "{elements:?}");

    let TemplateElement::Expression(expression) = &elements[0] else {
        panic!("expected an expression, got {:?}", elements[0]);
    };
    assert!(matches!(expression.name, Parameter::Path(_)), "{:?}", expression.name);

    let TemplateElement::Expression(call) = &elements[1] else {
        panic!("expected a call, got {:?}", elements[1]);
    };
    assert_eq!(call.name, Parameter::Name("env".to_string()));
    assert_eq!(call.params, vec![Parameter::Literal("HOME".into())]);
    assert!(call.template.is_none());

    let TemplateElement::HelperBlock(patch) = &elements[2] else {
        panic!("expected a block, got {:?}", elements[2]);
    };
    assert_eq!(patch.name, Parameter::Name("patch".to_string()));
    assert!(patch.params.is_empty(), "a patch block takes no positional parameters");
    assert_eq!(patch.hash.get("at"), Some(&Parameter::Literal("A".into())));
    assert_eq!(patch.hash.get("to"), Some(&Parameter::Literal("B".into())));
    let body = patch.template.as_ref().unwrap();
    assert_eq!(body.elements, vec![TemplateElement::RawString("body".to_string())]);

    assert_eq!(elements[3], TemplateElement::Comment(" note".to_string()));
    assert!(matches!(elements[4], TemplateElement::PartialExpression(_)));
    assert!(matches!(elements[5], TemplateElement::DecoratorExpression(_)));
}

/// Triple braces are a different element kind from single braces, which is how the content
/// layer refuses a construct it does not define.
#[test]
fn triple_braces_are_a_different_element_kind() {
    let template = Template::compile("{{{x}}}").unwrap();
    assert!(matches!(template.elements[0], TemplateElement::HtmlExpression(_)));
}

/// Two adjacent tags produce two elements, not one: the reporting unit is the element index.
#[test]
fn adjacent_tags_are_separate_elements() {
    let template = Template::compile("{{!a}}{{!b}}").unwrap();
    assert_eq!(template.elements.len(), 2);
}

/// Strict mode turns a missing value into a render error that carries a line and column, which
/// is what an `E-R1` block needs in order to point at the tag.
#[test]
fn a_missing_value_in_strict_mode_is_a_render_error_with_a_position() {
    let mut registry = Handlebars::new();
    registry.set_strict_mode(true);
    let template = Template::compile("a\n{{missing}}").unwrap();
    let context = Context::wraps(HashMap::<&str, &str>::new()).unwrap();
    let mut render_context = RenderContext::new(None);
    let error = template.renders(&registry, &context, &mut render_context).unwrap_err();
    assert_eq!((error.line_no, error.column_no), (Some(2), Some(1)));
}

/// A name that is not on the path is an error, never a silent empty answer: `which` answers
/// "where is this binary", and a missing binary has no answer.
#[test]
fn which_reports_a_missing_name_as_an_error() {
    assert!(which::which("tuckin-no-such-binary-4f1c").is_err());
}

#[cfg(windows)]
#[test]
fn which_finds_an_executable_every_windows_has() {
    assert!(which::which("cmd.exe").is_ok());
}

#[cfg(unix)]
#[test]
fn which_finds_an_executable_every_unix_has() {
    assert!(which::which("sh").is_ok());
}

/// A duplicate hash key is not a parse error: the map keeps one value. The AST therefore cannot
/// witness a duplicate, which is why duplicate-parameter detection stays a text-level refusal.
#[test]
fn duplicate_hash_keys_collapse_in_the_ast() {
    let template = Template::compile(r#"{{#patch at="a" at="b"}}x{{/patch}}"#).unwrap();
    let TemplateElement::HelperBlock(block) = &template.elements[0] else {
        panic!("{:?}", template.elements[0]);
    };
    assert_eq!(block.hash.len(), 1);
}

/// `~` is consumed as whitespace control and leaves no node or flag in the parsed template; its
/// only trace is that adjacent raw strings lose the spaces it trims. A closure check reading only
/// the AST therefore cannot refuse it: `a{{~x}}` is byte-identical to `a{{x}}`.
#[test]
fn whitespace_control_shows_only_as_trimmed_raw_strings() {
    let plain = Template::compile(" a {{x}} b ").unwrap();
    let controlled = Template::compile(" a {{~x~}} b ").unwrap();
    assert_eq!(plain.elements.len(), controlled.elements.len());
    assert_eq!(controlled.elements[0], TemplateElement::RawString(" a".to_string()));
    assert_eq!(controlled.elements[2], TemplateElement::RawString("b ".to_string()));
}

/// The two empty bodies the spec separates are distinguishable in the AST: tags written back to
/// back leave no element at all, while a body whose only byte is the trimmed newline leaves an
/// empty raw string. A deletion and a body that rendered to zero bytes are therefore not the
/// same observation, and the second can be refused where the first is accepted.
#[test]
fn an_empty_body_and_a_trimmed_newline_are_different_shapes() {
    let element_of = |source: &str| {
        let template = Template::compile(source).unwrap();
        let TemplateElement::HelperBlock(block) = &template.elements[0] else {
            panic!("{:?}", template.elements[0]);
        };
        block.template.as_ref().unwrap().elements.clone()
    };

    assert!(element_of(r#"{{#patch at="a"}}{{/patch}}"#).is_empty());
    assert_eq!(
        element_of("{{#patch at=\"a\"}}\n{{/patch}}"),
        vec![TemplateElement::RawString(String::new())]
    );
    assert_eq!(
        element_of(r#"{{#patch at="a"}}   {{/patch}}"#),
        vec![TemplateElement::RawString("   ".to_string())]
    );
}

/// A raw block is still a `HelperBlock`, under the name `raw`; refusing it by name is possible,
/// but its body is raw text, so it is also the second way to write a literal `{{`.
#[test]
fn a_raw_block_is_a_helper_block_named_raw() {
    let template = Template::compile("{{{{raw}}}}x{{{{/raw}}}}").unwrap();
    let TemplateElement::HelperBlock(block) = &template.elements[0] else {
        panic!("{:?}", template.elements[0]);
    };
    assert_eq!(block.name, Parameter::Name("raw".to_string()));
}

/// The legacy `{{& x}}` spelling is the same node as `{{{ x }}}`: one kind, two spellings, so a
/// closure rule on the kind refuses both without a second text rule.
#[test]
fn the_legacy_ampersand_is_the_unescaped_expression_kind() {
    let template = Template::compile("{{&x}}").unwrap();
    assert!(matches!(template.elements[0], TemplateElement::HtmlExpression(_)));
}

/// Both quote styles are one syntax: the parser turns either into the same string literal, so
/// `at='{"a":1}'` needs no per-character escaping and one rule covers both.
#[test]
fn both_quote_styles_are_the_same_string_literal() {
    let double = Template::compile(r#"{{#patch at="A"}}b{{/patch}}"#).unwrap();
    let single = Template::compile(r#"{{#patch at='A'}}b{{/patch}}"#).unwrap();
    assert_eq!(double.elements, single.elements);
}

/// `HelperTemplate` is the node a closure check walks; a block and a plain expression are the
/// same node type and are told apart by whether a body is present.
#[test]
fn a_block_and_an_expression_share_the_node_type_and_differ_by_template() {
    let template = Template::compile("{{x}}{{#y}}b{{/y}}").unwrap();
    let expression: &HelperTemplate = match &template.elements[0] {
        TemplateElement::Expression(e) => e,
        other => panic!("{other:?}"),
    };
    let block: &HelperTemplate = match &template.elements[1] {
        TemplateElement::HelperBlock(b) => b,
        other => panic!("{other:?}"),
    };
    assert!(expression.template.is_none());
    assert!(block.template.is_some());
}
