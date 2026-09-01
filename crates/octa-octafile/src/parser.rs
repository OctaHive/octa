use std::collections::HashMap;

use serde_yml::{Mapping, Number, Tag as ValueTag, TaggedValue, Value};
use yaml_rust2::{
  parser::{Event, MarkedEventReceiver, Parser, Tag},
  scanner::{Marker, TScalarStyle},
  Yaml,
};

#[derive(Clone, Debug)]
/// YAML node that keeps the source marker and tag discarded by the regular yaml-rust2 DOM loader.
pub(crate) struct Node {
  value: NodeValue,
  tag: Option<Tag>,
  marker: Marker,
}

#[derive(Clone, Debug)]
enum NodeValue {
  Scalar(Value),
  Sequence(Vec<Node>),
  Mapping(Vec<(Node, Node)>),
}

impl Node {
  pub(crate) fn marker(&self) -> Marker {
    self.marker
  }

  pub(crate) fn annotation(&self) -> Option<&str> {
    // Only local tags such as `!shell` are task annotations. Resolved standard
    // YAML tags use the `tag:yaml.org,2002:` handle and retain normal YAML semantics.
    self
      .tag
      .as_ref()
      .filter(|tag| tag.handle == "!")
      .map(|tag| tag.suffix.as_str())
  }

  pub(crate) fn into_mapping(self) -> Option<Vec<(Node, Node)>> {
    match self.value {
      NodeValue::Mapping(entries) => Some(entries),
      _ => None,
    }
  }

  pub(crate) fn as_str(&self) -> Option<&str> {
    match &self.value {
      NodeValue::Scalar(Value::String(value)) => Some(value),
      _ => None,
    }
  }

  pub(crate) fn into_value(self) -> Result<Value, String> {
    self.convert(true)
  }

  pub(crate) fn into_untagged_value(self) -> Result<Value, String> {
    // The outer tag selects a task plugin and must not be forwarded as part of
    // the plugin parameters. Tags nested inside the payload are still preserved.
    self.convert(false)
  }

  fn convert(self, preserve_tag: bool) -> Result<Value, String> {
    let value = match self.value {
      NodeValue::Scalar(value) => value,
      NodeValue::Sequence(nodes) => {
        Value::Sequence(nodes.into_iter().map(Node::into_value).collect::<Result<Vec<_>, _>>()?)
      },
      NodeValue::Mapping(entries) => {
        let mut mapping = Mapping::new();

        for (key, value) in entries {
          let marker = key.marker;
          let key = key
            .into_value()?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| location_error(marker, "mapping keys must be strings"))?;

          if mapping.insert(key.clone(), value.into_value()?).is_some() {
            return Err(location_error(marker, &format!("duplicated key '{key}'")));
          }
        }

        Value::Mapping(mapping)
      },
    };

    if preserve_tag {
      if let Some(tag) = self.tag.filter(|tag| !is_standard_tag(tag)) {
        return Ok(Value::Tagged(Box::new(TaggedValue::new(
          ValueTag::new(format!("{}{}", tag.handle, tag.suffix)),
          value,
        ))));
      }
    }

    Ok(value)
  }
}

enum ContainerValue {
  Sequence(Vec<Node>),
  Mapping {
    entries: Vec<(Node, Node)>,
    // yaml-rust2 emits mapping keys and values as consecutive events. A pending
    // key is stored here until the next complete node arrives.
    key: Option<Node>,
  },
}

struct Container {
  value: ContainerValue,
  anchor: usize,
  tag: Option<Tag>,
  marker: Marker,
}

#[derive(Default)]
struct Loader {
  documents: Vec<Node>,
  // Open sequences and mappings. Completed child nodes are attached to the
  // container at the top of the stack.
  stack: Vec<Container>,
  // Anchors point to already completed nodes so aliases can clone the same tree.
  anchors: HashMap<usize, Node>,
  // MarkedEventReceiver cannot return errors, so structural errors are retained
  // here and returned after yaml-rust2 finishes emitting events.
  error: Option<String>,
}

impl Loader {
  fn push_node(&mut self, node: Node, anchor: usize) {
    if self.error.is_some() {
      return;
    }

    if anchor > 0 {
      // yaml-rust2 uses zero to mean that the node has no anchor.
      self.anchors.insert(anchor, node.clone());
    }

    let Some(container) = self.stack.last_mut() else {
      self.documents.push(node);
      return;
    };

    match &mut container.value {
      ContainerValue::Sequence(nodes) => nodes.push(node),
      ContainerValue::Mapping { entries, key } => {
        if let Some(key) = key.take() {
          entries.push((key, node));
        } else {
          *key = Some(node);
        }
      },
    }
  }

  fn close_container(&mut self, sequence: bool, marker: Marker) {
    // Collection tags and markers arrive on the start event, so they are kept
    // in Container until the matching end event produces a complete Node.
    let Some(container) = self.stack.pop() else {
      self.error = Some(location_error(marker, "unexpected collection end"));
      return;
    };

    let value = match container.value {
      ContainerValue::Sequence(nodes) if sequence => NodeValue::Sequence(nodes),
      ContainerValue::Mapping { entries, key: None } if !sequence => NodeValue::Mapping(entries),
      ContainerValue::Mapping { key: Some(_), .. } => {
        self.error = Some(location_error(marker, "mapping value is missing"));
        return;
      },
      _ => {
        self.error = Some(location_error(marker, "unexpected collection end"));
        return;
      },
    };

    self.push_node(
      Node {
        value,
        tag: container.tag,
        marker: container.marker,
      },
      container.anchor,
    );
  }
}

impl MarkedEventReceiver for Loader {
  fn on_event(&mut self, event: Event, marker: Marker) {
    if self.error.is_some() {
      return;
    }

    match event {
      // Document boundaries do not affect the node stack; root nodes are added
      // directly to `documents` by push_node.
      Event::StreamStart | Event::StreamEnd | Event::DocumentStart | Event::DocumentEnd | Event::Nothing => {},
      Event::Scalar(value, style, anchor, tag) => {
        let scalar = scalar_value(&value, style, tag.as_ref());
        self.push_node(
          Node {
            value: NodeValue::Scalar(scalar),
            tag,
            marker,
          },
          anchor,
        );
      },
      Event::SequenceStart(anchor, tag) => self.stack.push(Container {
        value: ContainerValue::Sequence(Vec::new()),
        anchor,
        tag,
        marker,
      }),
      Event::SequenceEnd => self.close_container(true, marker),
      Event::MappingStart(anchor, tag) => self.stack.push(Container {
        value: ContainerValue::Mapping {
          entries: Vec::new(),
          key: None,
        },
        anchor,
        tag,
        marker,
      }),
      Event::MappingEnd => self.close_container(false, marker),
      Event::Alias(anchor) => match self.anchors.get(&anchor).cloned() {
        Some(node) => self.push_node(node, 0),
        None => self.error = Some(location_error(marker, "unknown YAML alias")),
      },
    }
  }
}

pub(crate) fn parse(content: &str) -> Result<Node, String> {
  let mut loader = Loader::default();
  Parser::new_from_str(content)
    .load(&mut loader, true)
    .map_err(|error| error.to_string())?;

  if let Some(error) = loader.error {
    return Err(error);
  }

  match loader.documents.len() {
    1 => Ok(loader.documents.pop().unwrap()),
    0 => Err("empty YAML document".to_string()),
    _ => Err("Octafile must contain a single YAML document".to_string()),
  }
}

fn scalar_value(value: &str, style: TScalarStyle, tag: Option<&Tag>) -> Value {
  // Quoted and block scalars are always strings, regardless of their contents.
  if style != TScalarStyle::Plain {
    return Value::String(value.to_owned());
  }

  if let Some(tag) = tag.filter(|tag| is_standard_tag(tag)) {
    // Explicit standard tags take precedence over implicit scalar inference.
    return match tag.suffix.as_str() {
      "bool" => match value {
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        _ => Value::String(value.to_owned()),
      },
      "int" => value
        .parse::<i64>()
        .map(|value| Value::Number(value.into()))
        .unwrap_or_else(|_| Value::String(value.to_owned())),
      "float" => value
        .parse::<f64>()
        .map(|value| Value::Number(Number::from(value)))
        .unwrap_or_else(|_| Value::String(value.to_owned())),
      "null" if matches!(value, "~" | "null" | "Null" | "NULL") => Value::Null,
      _ => Value::String(value.to_owned()),
    };
  }

  // Let yaml-rust2 apply its YAML scalar rules to untagged and locally tagged
  // plain values before converting them to the Value type used by Octafile.
  yaml_value(Yaml::from_str(value))
}

fn yaml_value(value: Yaml) -> Value {
  match value {
    Yaml::Integer(value) => Value::Number(value.into()),
    Yaml::Real(value) => value
      .parse::<f64>()
      .map(|value| Value::Number(Number::from(value)))
      .unwrap_or_else(|_| Value::String(value)),
    Yaml::Boolean(value) => Value::Bool(value),
    Yaml::Null | Yaml::BadValue => Value::Null,
    Yaml::String(value) => Value::String(value),
    _ => unreachable!("scalar parsing cannot produce a collection or alias"),
  }
}

fn is_standard_tag(tag: &Tag) -> bool {
  tag.handle == "tag:yaml.org,2002:"
}

pub(crate) fn location_error(marker: Marker, message: &str) -> String {
  format!("{message} at line {}, column {}", marker.line(), marker.col() + 1)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn standard_tag(suffix: &str) -> Tag {
    Tag {
      handle: "tag:yaml.org,2002:".to_owned(),
      suffix: suffix.to_owned(),
    }
  }

  #[test]
  fn parses_annotations_collections_and_aliases() {
    let node = parse(
      r#"!shell
command: run
nested: !tpl value
items: &items [one, 2]
copy: *items
"#,
    )
    .unwrap();

    assert_eq!(node.annotation(), Some("shell"));
    assert_eq!(node.marker().line(), 2);
    let value = node.into_untagged_value().unwrap();
    let mapping = value.as_mapping().unwrap();
    assert!(matches!(mapping.get("nested"), Some(Value::Tagged(_))));
    assert_eq!(mapping.get("items"), mapping.get("copy"));

    let tagged = parse("!shell echo hello").unwrap().into_value().unwrap();
    assert!(matches!(tagged, Value::Tagged(_)));
  }

  #[test]
  fn exposes_node_shapes_without_converting_them() {
    let mapping = parse("key: value").unwrap();
    assert!(mapping.clone().into_mapping().is_some());
    assert_eq!(mapping.as_str(), None);

    let scalar = parse("value").unwrap();
    assert_eq!(scalar.as_str(), Some("value"));
    assert!(scalar.into_mapping().is_none());
  }

  #[test]
  fn reports_document_and_mapping_errors() {
    assert_eq!(parse("").unwrap_err(), "empty YAML document");
    assert_eq!(
      parse("---\none\n---\ntwo\n").unwrap_err(),
      "Octafile must contain a single YAML document"
    );
    assert!(parse("[").is_err());

    let duplicate = parse("key: one\nkey: two\n").unwrap().into_value().unwrap_err();
    assert!(duplicate.contains("duplicated key 'key'"));

    let invalid_key = parse("? [one, two]\n: value\n").unwrap().into_value().unwrap_err();
    assert!(invalid_key.contains("mapping keys must be strings"));
    assert!(parse("value: *missing\n").is_err());
  }

  #[test]
  fn converts_explicit_and_implicit_scalar_types() {
    let bool_tag = standard_tag("bool");
    assert_eq!(
      scalar_value("TRUE", TScalarStyle::Plain, Some(&bool_tag)),
      Value::Bool(true)
    );
    assert_eq!(
      scalar_value("false", TScalarStyle::Plain, Some(&bool_tag)),
      Value::Bool(false)
    );
    assert_eq!(
      scalar_value("not-a-bool", TScalarStyle::Plain, Some(&bool_tag)),
      Value::String("not-a-bool".to_owned())
    );

    let int_tag = standard_tag("int");
    assert_eq!(
      scalar_value("42", TScalarStyle::Plain, Some(&int_tag)),
      Value::Number(42.into())
    );
    assert_eq!(
      scalar_value("invalid", TScalarStyle::Plain, Some(&int_tag)),
      Value::String("invalid".to_owned())
    );

    let float_tag = standard_tag("float");
    assert_eq!(
      scalar_value("1.5", TScalarStyle::Plain, Some(&float_tag)),
      Value::Number(Number::from(1.5))
    );
    assert_eq!(
      scalar_value("invalid", TScalarStyle::Plain, Some(&float_tag)),
      Value::String("invalid".to_owned())
    );

    let null_tag = standard_tag("null");
    assert_eq!(scalar_value("null", TScalarStyle::Plain, Some(&null_tag)), Value::Null);
    assert_eq!(
      scalar_value("value", TScalarStyle::Plain, Some(&null_tag)),
      Value::String("value".to_owned())
    );
    assert_eq!(
      scalar_value("42", TScalarStyle::DoubleQuoted, None),
      Value::String("42".to_owned())
    );

    assert_eq!(
      yaml_value(Yaml::Real("2.5".to_owned())),
      Value::Number(Number::from(2.5))
    );
    assert_eq!(
      yaml_value(Yaml::Real("invalid".to_owned())),
      Value::String("invalid".to_owned())
    );
    assert_eq!(yaml_value(Yaml::BadValue), Value::Null);
  }

  #[test]
  fn loader_retains_structural_errors() {
    let marker = parse("value").unwrap().marker();

    let mut loader = Loader::default();
    loader.on_event(Event::Alias(42), marker);
    assert!(loader.error.as_deref().unwrap().contains("unknown YAML alias"));

    let mut loader = Loader::default();
    loader.close_container(true, marker);
    assert!(loader.error.as_deref().unwrap().contains("unexpected collection end"));
    loader.push_node(parse("ignored").unwrap(), 1);
    loader.on_event(Event::Nothing, marker);
    assert!(loader.documents.is_empty());

    let mut loader = Loader::default();
    loader.stack.push(Container {
      value: ContainerValue::Mapping {
        entries: Vec::new(),
        key: Some(parse("key").unwrap()),
      },
      anchor: 0,
      tag: None,
      marker,
    });
    loader.close_container(false, marker);
    assert!(loader.error.as_deref().unwrap().contains("mapping value is missing"));

    let mut loader = Loader::default();
    loader.stack.push(Container {
      value: ContainerValue::Sequence(Vec::new()),
      anchor: 0,
      tag: None,
      marker,
    });
    loader.close_container(false, marker);
    assert!(loader.error.as_deref().unwrap().contains("unexpected collection end"));
  }
}
