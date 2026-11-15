//! The data plane: one flat map assembled from three layers.
//!
//! `[data]` is the default layer, `[data."@<os>"]` the platform layer, and `[data."@<tag>"]`
//! one layer per tag listed in `tuckin.tags`. Later layers win; the platform layer is
//! always in place; a tag with no layer is simply absent, because one configuration is shared
//! across machines and the layers of the others are expected to be there.
//!
//! The shape is checked here, at the one place where the plane is built: a flat map of scalars.
//! An array, a subtable that is not a layer, a nested layer or a date would either escape that
//! shape or be unreachable from a template, so each is `E-G1`.
//!
//! `tuckin.tags` declares the active tags for this working tree. One tag per line, `#` starts a
//! comment, and a missing file is an empty set.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use pulith::local::LocalPath;

use crate::diag::{Code, Diag};

pub(crate) const TAGS: &str = "tuckin.tags";

/// The merged plane. Values are scalars only; `split` proves that before this exists.
#[derive(Debug)]
pub struct Data {
    values: BTreeMap<String, toml::Value>,
}

impl Data {
    /// Merges the layers in the one fixed order: default, platform, then the tags in the order
    /// the machine listed them. A tag listed twice applies once, at its first position.
    pub fn load(data: Option<&toml::Table>, tags: &[String]) -> Result<Data, Diag> {
        let (defaults, layers) = split(data)?;
        let mut values = defaults;

        let platform = std::env::consts::OS;
        if let Some(layer) = layers.get(platform) {
            overlay(&mut values, layer);
        }
        let mut seen: HashSet<&str> = HashSet::from([platform]);
        for tag in tags {
            if !seen.insert(tag) {
                continue;
            }
            if let Some(layer) = layers.get(tag.as_str()) {
                overlay(&mut values, layer);
            }
        }
        Ok(Data { values })
    }

    pub fn values(&self) -> &BTreeMap<String, toml::Value> {
        &self.values
    }
}

fn overlay(values: &mut BTreeMap<String, toml::Value>, layer: &BTreeMap<String, toml::Value>) {
    values.extend(layer.iter().map(|(key, value)| (key.clone(), value.clone())));
}

/// `[data]` split into its default values and its layers. Every key starting with `@` must be a
/// table of scalars and every other key must be a scalar; anything else is `E-G1`.
type Layers = BTreeMap<String, BTreeMap<String, toml::Value>>;

fn split(table: Option<&toml::Table>) -> Result<(BTreeMap<String, toml::Value>, Layers), Diag> {
    let mut defaults = BTreeMap::new();
    let mut layers = Layers::new();
    let Some(table) = table else {
        return Ok((defaults, layers));
    };
    for (key, value) in table {
        match value {
            toml::Value::Table(inner) => {
                let tag = layer_name(key)?;
                layers.insert(tag.to_string(), scalars(tag, inner)?);
            }
            toml::Value::Array(_) => return Err(array(key)),
            toml::Value::Datetime(_) => return Err(datetime(key)),
            _ => {
                if key.starts_with('@') {
                    return Err(scalar_layer(key));
                }
                defaults.insert(key.clone(), value.clone());
            }
        }
    }
    Ok((defaults, layers))
}

fn layer_name(key: &str) -> Result<&str, Diag> {
    let Some(tag) = key.strip_prefix('@') else {
        return Err(shape(
            format!("data.{key}"),
            "a scalar value or a table named `@<tag>`",
            format!("\"{key}\" is a table without `@`"),
            "a nested table would be a second level, and the plane is flat",
            "give its values to the default layer, or name the table `\"@<tag>\"`",
        ));
    };
    if tag.is_empty() {
        return Err(shape(
            "data.\"@\"".to_string(),
            "a tag name after `@`",
            "the layer's name is empty",
            "the name after `@` matches a line in tuckin.tags",
            "write the tag, for example `[data.\"@work\"]`",
        ));
    }
    Ok(tag)
}

fn array(key: &str) -> Diag {
    shape(
        format!("data.{key}"),
        "one scalar value",
        format!("\"{key}\" is an array"),
        "a template name resolves to one value, so an array has no representation",
        "pick one value, or put the difference in another file",
    )
}

fn datetime(key: &str) -> Diag {
    shape(
        format!("data.{key}"),
        "a string, an integer, a float or a boolean",
        format!("\"{key}\" is a date"),
        "a date is not one of the four scalar types the plane carries",
        "write it as a quoted string, e.g. \"2026-11-13\"",
    )
}

fn scalar_layer(key: &str) -> Diag {
    shape(
        format!("data.{key}"),
        "a table `[data.\"@<tag>\"]`",
        format!("\"{key}\" is a scalar"),
        "a name starting with `@` is a layer, not a variable; no template can read it",
        "make it a table, or rename the variable without `@`",
    )
}

/// The inside of one layer: scalars, one level, no nested table.
fn scalars(tag: &str, inner: &toml::Table) -> Result<BTreeMap<String, toml::Value>, Diag> {
    let mut layer = BTreeMap::new();
    for (key, value) in inner {
        match value {
            toml::Value::String(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_) => {
                layer.insert(key.clone(), value.clone());
            }
            _ => {
                return Err(shape(
                    format!("data.\"@{tag}\".{key}"),
                    "a scalar value",
                    format!("\"{key}\" is not a string, an integer, a float or a boolean"),
                    "a layer is one level deep and holds values, not structure",
                    "flatten it, or give the nested map its own tag",
                ));
            }
        }
    }
    Ok(layer)
}

fn shape(
    subject: String,
    missing: impl Into<String>,
    current: impl Into<String>,
    why: impl Into<String>,
    fix: impl Into<String>,
) -> Diag {
    Diag::new(Code::G1, format!("tuckin.toml: {subject}"))
        .missing(missing)
        .current(current)
        .why(why)
        .fix(fix)
}

/// The tag list in file order. A missing file is an empty set; unreadable or non-UTF-8 bytes
/// are failures, because "no tags" and "cannot read the tags" are different facts.
pub(crate) fn read_tags(root: &Path) -> Result<Vec<String>, Diag> {
    let path = root.join(TAGS);
    let Some(mut file) = LocalPath::new(&path)
        .and_then(LocalPath::open_file)
        .map_err(|error| tags_unreadable(&path, error))?
    else {
        return Ok(Vec::new());
    };
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|error| tags_unreadable(&path, error))?;
    Ok(parse_tags(&text))
}

/// One tag per line, `#` starts a comment, blank lines are skipped. Duplicates survive here:
/// "applied once at the first position" is the merge's rule, not the file's. A leading byte
/// order mark is an encoding artifact of the editor, not part of the first tag.
fn parse_tags(text: &str) -> Vec<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines()
        .filter_map(|line| {
            let body = line.split('#').next().unwrap_or_default().trim();
            (!body.is_empty()).then(|| body.to_string())
        })
        .collect()
}

fn tags_unreadable(path: &Path, error: impl std::fmt::Display) -> Diag {
    Diag::new(Code::G1, path.display().to_string())
        .missing("a readable UTF-8 regular file, or no tags file")
        .current(error.to_string())
        .why("these tags select the active rules for this working tree")
        .fix("repair tuckin.tags, or remove it to use no tags")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const OS: &str = std::env::consts::OS;

    fn table(text: &str) -> Option<toml::Table> {
        Config::parse(text).expect("a valid configuration").data
    }

    fn load(text: &str, tags: &[&str]) -> Data {
        let tags: Vec<String> = tags.iter().map(|tag| tag.to_string()).collect();
        Data::load(table(text).as_ref(), &tags).expect("a valid plane")
    }

    fn value(data: &Data, key: &str) -> String {
        match data.values().get(key) {
            Some(toml::Value::String(text)) => text.clone(),
            other => panic!("no string value for `{key}`: {other:?}"),
        }
    }

    fn refused(text: &str) -> Diag {
        Data::load(table(text).as_ref(), &[]).expect_err("a shape this plane does not have")
    }

    #[test]
    fn the_default_layer_is_the_whole_plane_when_no_tag_applies() {
        let data = load("[data]\nshell = \"sh\"\n", &[]);
        assert_eq!(data.values().len(), 1);
        assert_eq!(value(&data, "shell"), "sh");
    }

    #[test]
    fn the_platform_layer_overrides_the_default() {
        let data =
            load(&format!("[data]\nshell = \"sh\"\n[data.\"@{OS}\"]\nshell = \"platform\"\n"), &[]);
        assert_eq!(value(&data, "shell"), "platform");
    }

    #[test]
    fn a_user_tag_layer_overrides_the_platform_layer() {
        let data = load(
            &format!(
                "[data]\nshell = \"sh\"\n[data.\"@{OS}\"]\nshell = \"platform\"\n[data.\"@work\"]\nshell = \"work\"\n"
            ),
            &["work"],
        );
        assert_eq!(value(&data, "shell"), "work");
    }

    #[test]
    fn user_tags_apply_in_the_order_they_are_listed() {
        let data = load(
            "[data]\nx = \"default\"\n[data.\"@a\"]\nx = \"a\"\n[data.\"@b\"]\nx = \"b\"\n",
            &["a", "b"],
        );
        assert_eq!(value(&data, "x"), "b");
    }

    #[test]
    fn a_repeated_tag_applies_once_at_its_first_position() {
        let data = load(
            "[data]\nx = \"default\"\n[data.\"@a\"]\nx = \"a\"\n[data.\"@b\"]\nx = \"b\"\n",
            &["a", "b", "a"],
        );
        assert_eq!(value(&data, "x"), "b");
    }

    #[test]
    fn a_tag_with_no_layer_is_simply_absent() {
        let data = load("[data]\nx = \"default\"\n", &["not-in-the-config"]);
        assert_eq!(value(&data, "x"), "default");
    }

    #[test]
    fn the_platform_name_in_the_tag_list_is_the_same_layer() {
        let data = load(
            &format!("[data]\nshell = \"sh\"\n[data.\"@{OS}\"]\nshell = \"platform\"\n"),
            &[OS],
        );
        assert_eq!(value(&data, "shell"), "platform");
    }

    #[test]
    fn the_four_scalar_types_are_all_carried() {
        let data = load("[data]\ns = \"x\"\ni = 3\nf = 1.5\nb = true\n", &[]);
        assert!(matches!(data.values().get("s"), Some(toml::Value::String(_))));
        assert!(matches!(data.values().get("i"), Some(toml::Value::Integer(3))));
        assert!(matches!(data.values().get("f"), Some(toml::Value::Float(_))));
        assert!(matches!(data.values().get("b"), Some(toml::Value::Boolean(true))));
    }

    #[test]
    fn an_array_value_is_g1() {
        let err = refused("[data]\npaths = [\"a\", \"b\"]\n");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("array"));
    }

    #[test]
    fn a_subtable_without_an_at_is_g1() {
        let err = refused("[data.nested]\nx = \"a\"\n");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("without `@`"));
    }

    #[test]
    fn a_nested_table_in_a_layer_is_g1() {
        let err = refused("[data.\"@work\".nested]\nx = \"a\"\n");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("one level deep"));
    }

    #[test]
    fn a_date_is_g1() {
        let err = refused("[data]\nstamp = 2026-11-13\n");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("date"));
    }

    #[test]
    fn a_scalar_under_an_at_name_is_g1() {
        let err = refused("[data]\n\"@work\" = \"x\"\n");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("`@` is a layer"));
    }

    #[test]
    fn an_empty_tag_name_is_g1() {
        let err = refused("[data.\"@\"]\nx = \"a\"\n");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("a tag name after `@`"));
    }

    #[test]
    fn a_missing_tags_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_tags(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        assert_eq!(
            parse_tags("# machine tags\n\nwork\n  # indented comment\nlaptop  # laptop\n"),
            vec!["work".to_string(), "laptop".to_string()]
        );
    }

    #[test]
    fn tag_order_is_file_order_and_duplicates_survive() {
        let tags = parse_tags("a\nb\na\n");
        assert_eq!(tags, vec!["a".to_string(), "b".to_string(), "a".to_string()]);
    }

    #[test]
    fn a_leading_byte_order_mark_is_not_part_of_the_first_tag() {
        assert_eq!(parse_tags("\u{feff}work\n"), vec!["work".to_string()]);
    }

    #[test]
    fn the_plane_is_what_the_template_context_sees() {
        let data = load("[data]\nshell = \"sh\"\n", &[]);
        let renderer = crate::content::Renderer::new(data.values()).expect("the plane serialises");
        match renderer.render("home/x", b"{{shell}}").expect("a defined key") {
            crate::content::Rendered::Whole(bytes) => assert_eq!(bytes, b"sh"),
            crate::content::Rendered::Sealed(_) | crate::content::Rendered::Patch(_) => {
                panic!("no block in this source")
            }
        }
    }

    #[test]
    fn a_non_utf8_tags_file_is_g1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TAGS);
        std::fs::write(&path, [0xff, 0xfe, b'a']).unwrap();
        let err = read_tags(dir.path()).expect_err("not UTF-8");
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("UTF-8"));
    }

    #[test]
    fn a_directory_in_place_of_the_tags_file_is_g1() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(TAGS)).unwrap();
        let err = read_tags(dir.path()).expect_err("a directory is not a tags file");
        assert_eq!(err.code(), Code::G1);
    }
}
