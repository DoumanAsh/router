use crate::prelude::graphql::*;
use serde::{Deserialize, Serialize};
pub use serde_json_bytes::Value;
use serde_json_bytes::{ByteString, Entry, Map};
use std::cmp::min;
use std::fmt;

/// A JSON object.
pub type Object = Map<ByteString, Value>;

/// NOT PUBLIC API
#[doc(hidden)]
#[macro_export]
macro_rules! extract_key_value_from_object {
    ($object:expr, $key:literal, $pattern:pat => $var:ident) => {{
        match $object.remove($key) {
            Some($pattern) => Ok(Some($var)),
            None | Some(Value::Null) => Ok(None),
            _ => Err(concat!("invalid type for key: ", $key)),
        }
    }};
    ($object:expr, $key:literal) => {{
        match $object.remove($key) {
            None | Some(Value::Null) => None,
            Some(value) => Some(value),
        }
    }};
}

/// NOT PUBLIC API
#[doc(hidden)]
#[macro_export]
macro_rules! ensure_object {
    ($value:expr) => {{
        match $value {
            Value::Object(o) => Ok(o),
            _ => Err("invalid type, expected an object"),
        }
    }};
}

#[doc(hidden)]
/// Extension trait for [`serde_json::Value`].
pub trait ValueExt {
    /// Deep merge the JSON objects, array and override the values in `&mut self` if they already
    /// exists.
    #[track_caller]
    fn deep_merge(&mut self, other: Self);

    /// Returns `true` if the values are equal and the objects are ordered the same.
    ///
    /// **Note:** this is recursive.
    fn eq_and_ordered(&self, other: &Self) -> bool;

    /// Returns `true` if the set is a subset of another, i.e., `other` contains at least all the
    /// values in `self`.
    #[track_caller]
    fn is_subset(&self, superset: &Value) -> bool;

    /// Create a `Value` by inserting a value at a subpath.
    ///
    /// This will create objects, arrays and null nodes as needed if they
    /// are not present: the resulting Value is meant to be merged with an
    /// existing one that contains those nodes.
    #[track_caller]
    fn from_path(path: &Path, value: Value) -> Value;

    /// Insert a `value` at a `Path`
    #[track_caller]
    fn insert(&mut self, path: &Path, value: Value) -> Result<(), FetchError>;

    /// Select all values matching a `Path`.
    ///
    /// the function passed as argument will be called with the values found and their Path
    /// if it encounters an invalid value, it will ignore it and continue
    #[track_caller]
    fn select_values_and_paths<'a, F>(&'a self, path: &'a Path, f: F)
    where
        F: FnMut(Path, &'a Value);
}

impl ValueExt for Value {
    fn deep_merge(&mut self, other: Self) {
        match (self, other) {
            (Value::Object(a), Value::Object(b)) => {
                for (key, value) in b.into_iter() {
                    match a.entry(key) {
                        Entry::Vacant(e) => {
                            e.insert(value);
                        }
                        Entry::Occupied(e) => {
                            e.into_mut().deep_merge(value);
                        }
                    }
                }
            }
            (Value::Array(a), Value::Array(mut b)) => {
                for (b_value, a_value) in b.drain(..min(a.len(), b.len())).zip(a.iter_mut()) {
                    a_value.deep_merge(b_value);
                }

                a.extend(b.into_iter());
            }
            (_, Value::Null) => {}
            (Value::Object(_), Value::Array(_)) => {
                failfast_debug!("trying to replace an object with an array");
            }
            (Value::Array(_), Value::Object(_)) => {
                failfast_debug!("trying to replace an array with an object");
            }
            (a, b) => {
                if b != Value::Null {
                    *a = b;
                }
            }
        }
    }

    fn eq_and_ordered(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Object(a), Value::Object(b)) => {
                let mut it_a = a.iter();
                let mut it_b = b.iter();

                loop {
                    match (it_a.next(), it_b.next()) {
                        (Some(_), None) | (None, Some(_)) => break false,
                        (None, None) => break true,
                        (Some((field_a, value_a)), Some((field_b, value_b)))
                            if field_a == field_b && ValueExt::eq_and_ordered(value_a, value_b) =>
                        {
                            continue
                        }
                        (Some(_), Some(_)) => break false,
                    }
                }
            }
            (Value::Array(a), Value::Array(b)) => {
                let mut it_a = a.iter();
                let mut it_b = b.iter();

                loop {
                    match (it_a.next(), it_b.next()) {
                        (Some(_), None) | (None, Some(_)) => break false,
                        (None, None) => break true,
                        (Some(value_a), Some(value_b))
                            if ValueExt::eq_and_ordered(value_a, value_b) =>
                        {
                            continue
                        }
                        (Some(_), Some(_)) => break false,
                    }
                }
            }
            (a, b) => a == b,
        }
    }

    fn is_subset(&self, superset: &Value) -> bool {
        match (self, superset) {
            (Value::Object(subset), Value::Object(superset)) => {
                subset.iter().all(|(key, value)| {
                    if let Some(other) = superset.get(key) {
                        value.is_subset(other)
                    } else {
                        false
                    }
                })
            }
            (Value::Array(subset), Value::Array(superset)) => {
                subset.len() == superset.len()
                    && subset.iter().enumerate().all(|(index, value)| {
                        if let Some(other) = superset.get(index) {
                            value.is_subset(other)
                        } else {
                            false
                        }
                    })
            }
            (a, b) => a == b,
        }
    }

    #[track_caller]
    fn from_path(path: &Path, value: Value) -> Value {
        let mut res_value = Value::default();
        let mut current_node = &mut res_value;

        for p in path.iter() {
            match p {
                PathElement::Flatten => {
                    return res_value;
                }

                &PathElement::Index(index) => match current_node {
                    Value::Array(a) => {
                        for _ in 0..index {
                            a.push(Value::default());
                        }
                        a.push(Value::default());
                        current_node = a
                            .get_mut(index)
                            .expect("we just created the value at that index");
                    }
                    Value::Null => {
                        let mut a = Vec::new();
                        for _ in 0..index {
                            a.push(Value::default());
                        }
                        a.push(Value::default());

                        *current_node = Value::Array(a);
                        current_node = current_node
                            .as_array_mut()
                            .expect("current_node was just set to a Value::Array")
                            .get_mut(index)
                            .expect("we just created the value at that index");
                    }
                    other => unreachable!("unreachable node: {:?}", other),
                },
                PathElement::Key(k) => {
                    let mut m = Map::new();
                    m.insert(k.as_str(), Value::default());

                    *current_node = Value::Object(m);
                    current_node = current_node
                        .as_object_mut()
                        .expect("current_node was just set to a Value::Object")
                        .get_mut(k.as_str())
                        .expect("the value at that key was just inserted");
                }
            }
        }

        *current_node = value;
        res_value
    }

    /// Insert a `value` at a `Path`
    #[track_caller]
    fn insert(&mut self, path: &Path, value: Value) -> Result<(), FetchError> {
        let mut current_node = self;

        for p in path.iter() {
            match p {
                PathElement::Flatten => {
                    if current_node.is_null() {
                        let a = Vec::new();
                        *current_node = Value::Array(a);
                    } else if !current_node.is_array() {
                        return Err(FetchError::ExecutionPathNotFound {
                            reason: "expected an array".to_string(),
                        });
                    }
                }

                &PathElement::Index(index) => match current_node {
                    Value::Array(a) => {
                        // add more elements if the index is after the end
                        for _ in a.len()..index + 1 {
                            a.push(Value::default());
                        }
                        current_node = a
                            .get_mut(index)
                            .expect("we just created the value at that index");
                    }
                    Value::Null => {
                        let mut a = Vec::new();
                        for _ in 0..index + 1 {
                            a.push(Value::default());
                        }

                        *current_node = Value::Array(a);
                        current_node = current_node
                            .as_array_mut()
                            .expect("current_node was just set to a Value::Array")
                            .get_mut(index)
                            .expect("we just created the value at that index");
                    }
                    _other => {
                        return Err(FetchError::ExecutionPathNotFound {
                            reason: "expected an array".to_string(),
                        })
                    }
                },
                PathElement::Key(k) => match current_node {
                    Value::Object(o) => {
                        current_node = o
                            .get_mut(k.as_str())
                            .expect("the value at that key was just inserted");
                    }
                    Value::Null => {
                        let mut m = Map::new();
                        m.insert(k.as_str(), Value::default());

                        *current_node = Value::Object(m);
                        current_node = current_node
                            .as_object_mut()
                            .expect("current_node was just set to a Value::Object")
                            .get_mut(k.as_str())
                            .expect("the value at that key was just inserted");
                    }
                    _other => {
                        return Err(FetchError::ExecutionPathNotFound {
                            reason: "expected an object".to_string(),
                        })
                    }
                },
            }
        }

        *current_node = value;
        Ok(())
    }

    #[track_caller]
    fn select_values_and_paths<'a, F>(&'a self, path: &'a Path, mut f: F)
    where
        F: FnMut(Path, &'a Value),
    {
        iterate_path(&Path::default(), &path.0, self, &mut f)
    }
}

fn iterate_path<'a, F>(parent: &Path, path: &'a [PathElement], data: &'a Value, f: &mut F)
where
    F: FnMut(Path, &'a Value),
{
    match path.get(0) {
        None => f(parent.clone(), data),
        Some(PathElement::Flatten) => {
            if let Some(array) = data.as_array() {
                for (i, value) in array.iter().enumerate() {
                    iterate_path(
                        &parent.join(Path::from(i.to_string())),
                        &path[1..],
                        value,
                        f,
                    );
                }
            }
        }
        Some(PathElement::Index(i)) => {
            if let Value::Array(a) = data {
                if let Some(value) = a.get(*i) {
                    iterate_path(
                        &parent.join(Path::from(i.to_string())),
                        &path[1..],
                        value,
                        f,
                    )
                }
            }
        }
        Some(PathElement::Key(k)) => {
            if let Value::Object(o) = data {
                if let Some(value) = o.get(k.as_str()) {
                    iterate_path(&parent.join(Path::from(k)), &path[1..], value, f)
                }
            }
        }
    }
}

/// A GraphQL path element that is composes of strings or numbers.
/// e.g `/book/3/name`
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathElement {
    /// A path element that given an array will flatmap the content.
    #[serde(
        deserialize_with = "deserialize_flatten",
        serialize_with = "serialize_flatten"
    )]
    Flatten,

    /// An index path element.
    Index(usize),

    /// A key path element.
    Key(String),
}

fn deserialize_flatten<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_str(FlattenVisitor)
}

struct FlattenVisitor;

impl<'de> serde::de::Visitor<'de> for FlattenVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a string that is '@'")
    }

    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if s == "@" {
            Ok(())
        } else {
            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(s),
                &self,
            ))
        }
    }
}

fn serialize_flatten<S>(serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str("@")
}

/// A path into the result document.
///
/// This can be composed of strings and numbers
#[doc(hidden)]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Path(pub Vec<PathElement>);

impl Path {
    pub fn from_slice<T: AsRef<str>>(s: &[T]) -> Self {
        Self(
            s.iter()
                .map(|x| x.as_ref())
                .map(|s| {
                    if let Ok(index) = s.parse::<usize>() {
                        PathElement::Index(index)
                    } else if s == "@" {
                        PathElement::Flatten
                    } else {
                        PathElement::Key(s.to_string())
                    }
                })
                .collect(),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = &PathElement> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn empty() -> Path {
        Path(Default::default())
    }

    pub fn parent(&self) -> Option<Path> {
        if self.is_empty() {
            None
        } else {
            Some(Path(self.iter().take(self.len() - 1).cloned().collect()))
        }
    }

    pub fn join(&self, other: impl AsRef<Self>) -> Self {
        let other = other.as_ref();
        let mut new = Vec::with_capacity(self.len() + other.len());
        new.extend(self.iter().cloned());
        new.extend(other.iter().cloned());
        Path(new)
    }
}

impl AsRef<Path> for Path {
    fn as_ref(&self) -> &Path {
        self
    }
}

impl<T> From<T> for Path
where
    T: AsRef<str>,
{
    fn from(s: T) -> Self {
        Self(
            s.as_ref()
                .split('/')
                .map(|s| {
                    if let Ok(index) = s.parse::<usize>() {
                        PathElement::Index(index)
                    } else if s == "@" {
                        PathElement::Flatten
                    } else {
                        PathElement::Key(s.to_string())
                    }
                })
                .collect(),
        )
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for element in self.iter() {
            write!(f, "/")?;
            match element {
                PathElement::Index(index) => write!(f, "{}", index)?,
                PathElement::Key(key) => write!(f, "{}", key)?,
                PathElement::Flatten => write!(f, "@")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json_bytes::json;

    macro_rules! assert_is_subset {
        ($a:expr, $b:expr $(,)?) => {
            assert!($a.is_subset(&$b));
        };
    }

    macro_rules! assert_is_not_subset {
        ($a:expr, $b:expr $(,)?) => {
            assert!(!$a.is_subset(&$b));
        };
    }

    fn select_values<'a>(path: &'a Path, data: &'a Value) -> Result<Vec<&'a Value>, FetchError> {
        let mut v = Vec::new();
        data.select_values_and_paths(path, |_path, value| {
            v.push(value);
        });
        Ok(v)
    }

    #[test]
    fn test_get_at_path() {
        let json = json!({"obj":{"arr":[{"prop1":1},{"prop1":2}]}});
        let path = Path::from("obj/arr/1/prop1");
        let result = select_values(&path, &json).unwrap();
        assert_eq!(result, vec![&Value::Number(2.into())]);
    }

    #[test]
    fn test_get_at_path_flatmap() {
        let json = json!({"obj":{"arr":[{"prop1":1},{"prop1":2}]}});
        let path = Path::from("obj/arr/@");
        let result = select_values(&path, &json).unwrap();
        assert_eq!(result, vec![&json!({"prop1":1}), &json!({"prop1":2})]);
    }

    #[test]
    fn test_get_at_path_flatmap_nested() {
        let json = json!({
            "obj": {
                "arr": [
                    {
                        "prop1": [
                            {"prop2": {"prop3": 1}, "prop4": -1},
                            {"prop2": {"prop3": 2}, "prop4": -2},
                        ],
                    },
                    {
                        "prop1": [
                            {"prop2": {"prop3": 3}, "prop4": -3},
                            {"prop2": {"prop3": 4}, "prop4": -4},
                        ],
                    },
                ],
            },
        });
        let path = Path::from("obj/arr/@/prop1/@/prop2");
        let result = select_values(&path, &json).unwrap();
        assert_eq!(
            result,
            vec![
                &json!({"prop3":1}),
                &json!({"prop3":2}),
                &json!({"prop3":3}),
                &json!({"prop3":4}),
            ],
        );
    }

    #[test]
    fn test_deep_merge() {
        let mut json = json!({"obj":{"arr":[{"prop1":1},{"prop2":2}]}});
        json.deep_merge(json!({"obj":{"arr":[{"prop1":2,"prop3":3},{"prop4":4}]}}));
        assert_eq!(
            json,
            json!({"obj":{"arr":[{"prop1":2, "prop3":3},{"prop2":2, "prop4":4}]}})
        );
    }

    #[test]
    fn test_is_subset_eq() {
        assert_is_subset!(
            json!({"obj":{"arr":[{"prop1":1},{"prop4":4}]}}),
            json!({"obj":{"arr":[{"prop1":1},{"prop4":4}]}}),
        );
    }

    #[test]
    fn test_is_subset_missing_pop() {
        assert_is_subset!(
            json!({"obj":{"arr":[{"prop1":1},{"prop4":4}]}}),
            json!({"obj":{"arr":[{"prop1":1,"prop3":3},{"prop4":4}]}}),
        );
    }

    #[test]
    fn test_is_subset_array_lengths_differ() {
        assert_is_not_subset!(
            json!({"obj":{"arr":[{"prop1":1}]}}),
            json!({"obj":{"arr":[{"prop1":1,"prop3":3},{"prop4":4}]}}),
        );
    }

    #[test]
    fn test_is_subset_extra_prop() {
        assert_is_not_subset!(
            json!({"obj":{"arr":[{"prop1":1,"prop3":3},{"prop4":4}]}}),
            json!({"obj":{"arr":[{"prop1":1},{"prop4":4}]}}),
        );
    }

    #[test]
    fn eq_and_ordered() {
        // test not objects
        assert!(json!([1, 2, 3]).eq_and_ordered(&json!([1, 2, 3])));
        assert!(!json!([1, 3, 2]).eq_and_ordered(&json!([1, 2, 3])));

        // test objects not nested
        assert!(json!({"foo":1,"bar":2}).eq_and_ordered(&json!({"foo":1,"bar":2})));
        assert!(!json!({"foo":1,"bar":2}).eq_and_ordered(&json!({"foo":1,"bar":3})));
        assert!(!json!({"foo":1,"bar":2}).eq_and_ordered(&json!({"foo":1,"bar":2,"baz":3})));
        assert!(!json!({"foo":1,"bar":2,"baz":3}).eq_and_ordered(&json!({"foo":1,"bar":2})));
        assert!(!json!({"bar":2,"foo":1}).eq_and_ordered(&json!({"foo":1,"bar":2})));

        // test objects nested
        assert!(json!({"baz":{"foo":1,"bar":2}}).eq_and_ordered(&json!({"baz":{"foo":1,"bar":2}})));
        assert!(!json!({"baz":{"bar":2,"foo":1}}).eq_and_ordered(&json!({"baz":{"foo":1,"bar":2}})));
        assert!(!json!([1,{"bar":2,"foo":1},2]).eq_and_ordered(&json!([1,{"foo":1,"bar":2},2])));
    }

    #[test]
    fn test_from_path() {
        let json = json!([{"prop1":1},{"prop1":2}]);
        let path = Path::from("obj/arr");
        let result = Value::from_path(&path, json);
        assert_eq!(result, json!({"obj":{"arr":[{"prop1":1},{"prop1":2}]}}));
    }

    #[test]
    fn test_from_path_index() {
        let json = json!({"prop1":1});
        let path = Path::from("obj/arr/1");
        let result = Value::from_path(&path, json);
        assert_eq!(result, json!({"obj":{"arr":[null, {"prop1":1}]}}));
    }

    #[test]
    fn test_from_path_flatten() {
        let json = json!({"prop1":1});
        let path = Path::from("obj/arr/@/obj2");
        let result = Value::from_path(&path, json);
        assert_eq!(result, json!({"obj":{"arr":null}}));
    }
}