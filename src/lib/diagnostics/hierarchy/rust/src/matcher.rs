// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{DiagnosticsHierarchy, Error, Property};
use fidl_fuchsia_diagnostics_common::{
    PropertySelector, Selector, StringSelector, StringSelectorUnknown, SubtreeSelector,
    TreeSelector,
};
use selectors::ValidateExt;
use std::borrow::{Borrow, Cow};
use std::cmp::Ordering;
use std::collections::BTreeMap;

impl<T: Borrow<Selector>> TryFrom<&[T]> for HierarchyMatcher {
    type Error = Error;

    fn try_from(selectors: &[T]) -> Result<Self, Self::Error> {
        // TODO(https://fxbug.dev/42069126: remove cloning, the archivist can probably hold
        // HierarchyMatcher<'static>
        let mut matcher = HierarchyMatcher::default();
        for selector in selectors {
            let selector = selector.borrow();
            selector.validate().map_err(|e| Error::Selectors(e.into()))?;

            // Safe to unwrap since we already validated the selector.
            // TODO(https://fxbug.dev/42069126): instead of doing this over Borrow<Selector> do it over
            // Selector.
            match selector.tree_selector.clone().unwrap() {
                TreeSelector::SubtreeSelector(subtree_selector) => {
                    matcher.insert_subtree(subtree_selector);
                }
                TreeSelector::PropertySelector(property_selector) => {
                    matcher.insert_property(property_selector);
                }
                _ => return Err(Error::Selectors(selectors::Error::InvalidTreeSelector)),
            }
        }
        Ok(matcher)
    }
}

impl<T: Borrow<Selector>> TryFrom<Vec<T>> for HierarchyMatcher {
    type Error = Error;

    fn try_from(selectors: Vec<T>) -> Result<Self, Self::Error> {
        selectors[..].try_into()
    }
}

#[derive(Debug)]
struct OrdStringSelector(StringSelector);

impl From<StringSelector> for OrdStringSelector {
    fn from(selector: StringSelector) -> Self {
        Self(selector)
    }
}

impl Ord for OrdStringSelector {
    fn cmp(&self, other: &OrdStringSelector) -> Ordering {
        match (&self.0, &other.0) {
            (StringSelector::ExactMatch(s), StringSelector::ExactMatch(o)) => s.cmp(o),
            (StringSelector::StringPattern(s), StringSelector::StringPattern(o)) => s.cmp(o),
            (StringSelector::ExactMatch(_), StringSelector::StringPattern(_)) => Ordering::Less,
            (StringSelector::StringPattern(_), StringSelector::ExactMatch(_)) => Ordering::Greater,
            (StringSelectorUnknown!(), StringSelector::ExactMatch(_)) => Ordering::Less,
            (StringSelectorUnknown!(), StringSelector::StringPattern(_)) => Ordering::Less,
            (StringSelectorUnknown!(), StringSelectorUnknown!()) => Ordering::Equal,
        }
    }
}

impl PartialOrd for OrdStringSelector {
    fn partial_cmp(&self, other: &OrdStringSelector) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for OrdStringSelector {
    fn eq(&self, other: &OrdStringSelector) -> bool {
        match (&self.0, &other.0) {
            (StringSelector::ExactMatch(s), StringSelector::ExactMatch(o)) => s.eq(o),
            (StringSelector::StringPattern(s), StringSelector::StringPattern(o)) => s.eq(o),
            (StringSelector::ExactMatch(_), StringSelector::StringPattern(_)) => false,
            (StringSelector::StringPattern(_), StringSelector::ExactMatch(_)) => false,
            (StringSelectorUnknown!(), StringSelectorUnknown!()) => true,
        }
    }
}

impl Eq for OrdStringSelector {}

#[derive(Default, Debug)]
pub struct HierarchyMatcher {
    nodes: BTreeMap<OrdStringSelector, HierarchyMatcher>,
    properties: Vec<OrdStringSelector>,
    subtree: bool,
}

impl HierarchyMatcher {
    pub fn new<I>(selectors: I) -> Result<Self, Error>
    where
        I: Iterator<Item = Selector>,
    {
        let mut matcher = HierarchyMatcher::default();
        for selector in selectors {
            selector.validate().map_err(|e| Error::Selectors(e.into()))?;

            // Safe to unwrap since we already validated the selector.
            match selector.tree_selector.unwrap() {
                TreeSelector::SubtreeSelector(subtree_selector) => {
                    matcher.insert_subtree(subtree_selector);
                }
                TreeSelector::PropertySelector(property_selector) => {
                    matcher.insert_property(property_selector);
                }
                _ => return Err(Error::Selectors(selectors::Error::InvalidTreeSelector)),
            }
        }
        Ok(matcher)
    }

    /// Builds a [`HierarchyMatcher`] by parsing selector strings (for example,
    /// `"*:root/foo:bar"` or `"*:root/subtree"`).
    pub fn from_selector_strs<I, S>(selectors: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let parsed = selectors
            .into_iter()
            .map(|s| selectors::parse_selector::<selectors::VerboseError>(s.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(parsed.into_iter())
    }

    fn insert_subtree(&mut self, selector: SubtreeSelector) {
        self.insert(selector.node_path, None);
    }

    fn insert_property(&mut self, selector: PropertySelector) {
        self.insert(selector.node_path, Some(selector.target_properties));
    }

    fn insert(&mut self, node_path: Vec<StringSelector>, property: Option<StringSelector>) {
        // Note: this could have additional optimization so that branches are collapsed into a
        // single one (for example foo/bar is included by f*o/bar), however, in practice, we don't
        // hit that edge case.
        let mut matcher = self;
        for node in node_path {
            matcher = matcher.nodes.entry(node.into()).or_default();
        }
        match property {
            Some(property) => {
                matcher.properties.push(property.into());
            }
            None => matcher.subtree = true,
        }
    }

    /// Returns whether a property named `property_name` located at `node_path` (including the root
    /// node segment, e.g. `["root", "foo"]`) would be retained when filtering a hierarchy with
    /// this [`HierarchyMatcher`].
    pub fn matches_property<S: AsRef<str>>(&self, node_path: &[S], property_name: &str) -> bool {
        if node_path.is_empty() {
            return false;
        }
        let mut current = vec![self];
        for segment in node_path {
            current = eval_matchers_on_node_name(segment.as_ref(), &current);
            if current.is_empty() {
                return false;
            }
            if current.iter().any(|m| m.subtree) {
                return true;
            }
        }
        eval_matchers_on_property(property_name, &current)
    }
}

#[derive(Debug, PartialEq)]
pub enum SelectResult<'a, Key: Clone> {
    Properties(Vec<Cow<'a, Property<Key>>>),
    Nodes(Vec<Cow<'a, DiagnosticsHierarchy<Key>>>),
}

impl<'a, Key: Clone> SelectResult<'a, Key> {
    pub fn into_owned(self) -> SelectResult<'static, Key> {
        match self {
            Self::Properties(v) => SelectResult::Properties(
                v.into_iter().map(|v| Cow::Owned(v.into_owned())).collect(),
            ),
            Self::Nodes(v) => {
                SelectResult::Nodes(v.into_iter().map(|v| Cow::Owned(v.into_owned())).collect())
            }
        }
    }

    /// Returns Err(()) if `self` is `Self::Nodes`. Otherwise, adds to property list.
    fn add_property(&mut self, prop: &'a Property<Key>) {
        let Self::Properties(v) = self else {
            panic!("must be Self::Properties to call add_property");
        };
        v.push(Cow::Borrowed(prop));
    }

    /// Returns Err(()) if `self` is `Self::Properties`. Otherwise, adds to property list.
    fn add_node(&mut self, node: &'a DiagnosticsHierarchy<Key>) {
        let Self::Nodes(v) = self else {
            panic!("must be Self::Nodes to call add_node");
        };
        v.push(Cow::Borrowed(node));
    }
}

pub fn select_from_hierarchy<'a, 'b, Key>(
    root_node: &'a DiagnosticsHierarchy<Key>,
    selector: &'b Selector,
) -> Result<SelectResult<'a, Key>, Error>
where
    Key: AsRef<str> + Clone,
    'a: 'b,
{
    selector.validate()?;

    struct StackEntry<'a, Key> {
        node: &'a DiagnosticsHierarchy<Key>,
        node_path_index: usize,
        explored_path: Vec<&'a str>,
    }

    // Safe to unwrap since we validated above.
    let (node_path, property_selector, stack_entry) = match selector.tree_selector.as_ref().unwrap()
    {
        TreeSelector::SubtreeSelector(subtree_selector) => (
            &subtree_selector.node_path,
            None,
            StackEntry { node: root_node, node_path_index: 0, explored_path: vec![] },
        ),
        TreeSelector::PropertySelector(property_selector) => (
            &property_selector.node_path,
            Some(&property_selector.target_properties),
            StackEntry { node: root_node, node_path_index: 0, explored_path: vec![] },
        ),
        _ => return Err(Error::InvalidTreeSelector),
    };

    let mut stack = vec![stack_entry];
    let mut result = if property_selector.is_some() {
        SelectResult::Properties(vec![])
    } else {
        SelectResult::Nodes(vec![])
    };

    while let Some(StackEntry { node, node_path_index, mut explored_path }) = stack.pop() {
        // Unwrap is safe since we validate is_empty right above.
        if !selectors::match_string(&node_path[node_path_index], &node.name) {
            continue;
        }
        explored_path.push(&node.name);

        // If we are at the last node in the path, then we just need to explore the properties.
        // Otherwise, we explore the children of the current node and the properties.
        if node_path_index != node_path.len() - 1 {
            // If this node matches the next selector we are looking at, then explore its children.
            for child in node.children.iter() {
                stack.push(StackEntry {
                    node: child,
                    node_path_index: node_path_index + 1,
                    explored_path: explored_path.clone(),
                });
            }
        } else if let Some(s) = property_selector {
            // If we have a property selector, then add any properties matching it to our result.
            for property in &node.properties {
                if selectors::match_string(s, property.key()) {
                    result.add_property(property);
                }
            }
        } else {
            // If we don't have a property selector and we reached the end of the node path, then
            // we should add the current node to the result.
            result.add_node(node);
        }
    }

    Ok(result)
}

/// Filters a hierarchy given a tree selector.
pub fn filter_tree<'a, Key, I: IntoIterator<Item = &'a TreeSelector>>(
    root_node: DiagnosticsHierarchy<Key>,
    selectors: I,
) -> Option<DiagnosticsHierarchy<Key>>
where
    Key: AsRef<str>,
{
    let mut matcher = HierarchyMatcher::default();
    for selector in selectors.into_iter() {
        match selector {
            TreeSelector::SubtreeSelector(subtree_selector) => {
                matcher.insert_subtree(subtree_selector.clone());
            }
            TreeSelector::PropertySelector(property_selector) => {
                matcher.insert_property(property_selector.clone());
            }
            _ => {}
        }
    }
    filter_hierarchy(root_node, &matcher)
}

/// Filters a diagnostics hierarchy using a set of path selectors and their associated property
/// selectors.
///
/// If the return type is None that implies that the filter encountered no errors AND the tree was
/// filtered to be empty at the end.
pub fn filter_hierarchy<Key>(
    mut root_node: DiagnosticsHierarchy<Key>,
    hierarchy_matcher: &HierarchyMatcher,
) -> Option<DiagnosticsHierarchy<Key>>
where
    Key: AsRef<str>,
{
    let starts_empty = root_node.children.is_empty() && root_node.properties.is_empty();
    if filter_hierarchy_helper(&mut root_node, &[hierarchy_matcher]) {
        if !starts_empty && root_node.children.is_empty() && root_node.properties.is_empty() {
            return None;
        }
        return Some(root_node);
    }
    None
}

fn filter_hierarchy_helper<Key>(
    node: &mut DiagnosticsHierarchy<Key>,
    hierarchy_matchers: &[&HierarchyMatcher],
) -> bool
where
    Key: AsRef<str>,
{
    let child_matchers = eval_matchers_on_node_name(&node.name, hierarchy_matchers);
    if child_matchers.is_empty() {
        node.children.clear();
        node.properties.clear();
        return false;
    }

    if child_matchers.iter().any(|m| m.subtree) {
        return true;
    }

    node.children.retain_mut(|child| filter_hierarchy_helper(child, &child_matchers));
    node.properties.retain_mut(|prop| eval_matchers_on_property(prop.name(), &child_matchers));

    !(node.children.is_empty() && node.properties.is_empty())
}

fn eval_matchers_on_node_name<'m>(
    node_name: &str,
    matchers: &[&'m HierarchyMatcher],
) -> Vec<&'m HierarchyMatcher> {
    let mut result = vec![];
    for matcher in matchers {
        for (node_pattern, tree_matcher) in matcher.nodes.iter() {
            if selectors::match_string(&node_pattern.0, node_name) {
                result.push(tree_matcher);
            }
        }
    }
    result
}

fn eval_matchers_on_property(property_name: &str, matchers: &[&HierarchyMatcher]) -> bool {
    matchers.iter().any(|matcher| {
        matcher
            .properties
            .iter()
            .any(|property_pattern| selectors::match_string(&property_pattern.0, property_name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use selectors::VerboseError;
    use std::sync::Arc;
    use test_case::test_case;

    fn parse_selectors_and_filter_hierarchy(
        hierarchy: DiagnosticsHierarchy,
        test_selectors: Vec<&str>,
    ) -> Option<DiagnosticsHierarchy> {
        let parsed_test_selectors = test_selectors
            .into_iter()
            .map(|selector_string| {
                Arc::new(
                    selectors::parse_selector::<VerboseError>(selector_string)
                        .expect("All test selectors are valid and parsable."),
                )
            })
            .collect::<Vec<Arc<Selector>>>();

        let hierarchy_matcher: HierarchyMatcher = parsed_test_selectors.try_into().unwrap();

        filter_hierarchy(hierarchy, &hierarchy_matcher).map(|mut hierarchy| {
            hierarchy.sort();
            hierarchy
        })
    }

    fn get_test_hierarchy() -> DiagnosticsHierarchy {
        DiagnosticsHierarchy::new(
            "root",
            vec![
                Property::String("x".to_string(), "foo".to_string()),
                Property::Uint("c".to_string(), 3),
                Property::Int("z".to_string(), -4),
            ],
            vec![
                make_foo(),
                DiagnosticsHierarchy::new(
                    "bar",
                    vec![Property::Int("12".to_string(), -4)],
                    vec![DiagnosticsHierarchy::new(
                        "zed",
                        vec![Property::Int("13/:".to_string(), -4)],
                        vec![],
                    )],
                ),
            ],
        )
    }

    fn make_all_foo_props() -> Vec<Property> {
        vec![
            Property::Int("11".to_string(), -4),
            Property::Bytes("123".to_string(), b"foo".to_vec()),
            Property::Double("0".to_string(), 8.1),
        ]
    }

    fn make_zed() -> Vec<DiagnosticsHierarchy> {
        vec![DiagnosticsHierarchy::new("zed", vec![Property::Int("13".to_string(), -4)], vec![])]
    }

    fn make_foo() -> DiagnosticsHierarchy {
        DiagnosticsHierarchy::new("foo", make_all_foo_props(), make_zed())
    }

    #[fuchsia::test]
    fn test_filter_hierarchy() {
        let test_selectors = vec!["*:root/foo:11", "*:root:z", r#"*:root/bar/zed:13\/\:"#];

        assert_eq!(
            parse_selectors_and_filter_hierarchy(get_test_hierarchy(), test_selectors),
            Some(DiagnosticsHierarchy::new(
                "root",
                vec![Property::Int("z".to_string(), -4),],
                vec![
                    DiagnosticsHierarchy::new(
                        "bar",
                        vec![],
                        vec![DiagnosticsHierarchy::new(
                            "zed",
                            vec![Property::Int("13/:".to_string(), -4)],
                            vec![],
                        )],
                    ),
                    DiagnosticsHierarchy::new(
                        "foo",
                        vec![Property::Int("11".to_string(), -4),],
                        vec![],
                    )
                ],
            ))
        );

        let test_selectors = vec!["*:root"];
        let mut sorted_expected = get_test_hierarchy();
        sorted_expected.sort();
        assert_eq!(
            parse_selectors_and_filter_hierarchy(get_test_hierarchy(), test_selectors),
            Some(sorted_expected)
        );
    }

    #[fuchsia::test]
    fn test_filter_does_not_include_empty_node() {
        let test_selectors = vec!["*:root/foo:blorg"];

        assert_eq!(
            parse_selectors_and_filter_hierarchy(get_test_hierarchy(), test_selectors),
            None,
        );
    }

    #[fuchsia::test]
    fn test_filter_empty_hierarchy() {
        let test_selectors = vec!["*:root"];

        assert_eq!(
            parse_selectors_and_filter_hierarchy(
                DiagnosticsHierarchy::new("root", vec![], vec![]),
                test_selectors
            ),
            Some(DiagnosticsHierarchy::new("root", vec![], vec![])),
        );
    }

    #[fuchsia::test]
    fn test_full_filtering() {
        // If we select a non-existent root, then we return a fully filtered hierarchy.
        let test_selectors = vec!["*:non-existent-root"];
        assert_eq!(
            parse_selectors_and_filter_hierarchy(get_test_hierarchy(), test_selectors),
            None,
        );

        // If we select a non-existent child of the root, then we return a fully filtered hierarchy.
        let test_selectors = vec!["*:root/i-dont-exist:foo"];
        assert_eq!(
            parse_selectors_and_filter_hierarchy(get_test_hierarchy(), test_selectors),
            None,
        );

        // Even if the root exists, but we don't include any property, we consider the hierarchy
        // fully filtered. This is aligned with the previous case.
        let test_selectors = vec!["*:root:i-dont-exist"];
        assert_eq!(
            parse_selectors_and_filter_hierarchy(get_test_hierarchy(), test_selectors),
            None,
        );
    }

    #[fuchsia::test]
    fn test_subtree_selection_includes_empty_nodes() {
        let test_selectors = vec!["*:root"];
        let mut empty_hierarchy = DiagnosticsHierarchy::new(
            "root",
            vec![],
            vec![
                DiagnosticsHierarchy::new(
                    "foo",
                    vec![],
                    vec![DiagnosticsHierarchy::new("zed", vec![], vec![])],
                ),
                DiagnosticsHierarchy::new(
                    "bar",
                    vec![],
                    vec![DiagnosticsHierarchy::new("zed", vec![], vec![])],
                ),
            ],
        );

        empty_hierarchy.sort();

        assert_eq!(
            parse_selectors_and_filter_hierarchy(empty_hierarchy.clone(), test_selectors),
            Some(empty_hierarchy)
        );
    }

    #[fuchsia::test]
    fn test_empty_tree_filtering() {
        // Subtree selection on the empty tree should produce the empty tree.
        let mut empty_hierarchy = DiagnosticsHierarchy::new("root", vec![], vec![]);
        empty_hierarchy.sort();

        let subtree_selector = vec!["*:root"];
        assert_eq!(
            parse_selectors_and_filter_hierarchy(empty_hierarchy.clone(), subtree_selector),
            Some(empty_hierarchy.clone())
        );

        // Selecting a property on the root, even if it doesn't exist, should produce nothing.
        let fake_property_selector = vec!["*:root:blorp"];
        assert_eq!(
            parse_selectors_and_filter_hierarchy(empty_hierarchy.clone(), fake_property_selector),
            None,
        );
    }

    #[test_case(vec![Property::Int("11".to_string(), -4)], "root/foo:11" ; "specific_property")]
    #[test_case(make_all_foo_props(), "root/foo:*" ; "many_properties")]
    #[test_case(vec![], "root/foo:none" ; "property_not_there")]
    #[fuchsia::test]
    fn test_select_from_hierarchy_property_selectors(expected: Vec<Property>, tree_selector: &str) {
        let hierarchy = get_test_hierarchy();
        let parsed_selector =
            selectors::parse_selector::<VerboseError>(&format!("*:{tree_selector}"))
                .expect("All test selectors are valid and parsable.");
        let Ok(SelectResult::Properties(mut property_entry_vec)) =
            select_from_hierarchy(&hierarchy, &parsed_selector)
        else {
            panic!("must be properties");
        };

        property_entry_vec.sort_by(|p1, p2| p1.name().cmp(p2.name()));
        let mut expected = expected.iter().map(Cow::Borrowed).collect::<Vec<_>>();
        expected.sort_by(|p1, p2| p1.name().cmp(p2.name()));

        assert_eq!(property_entry_vec, expected);
    }

    #[test_case(vec![], "root/none" ; "node_not_there")]
    #[test_case(make_zed(), "root/foo/zed" ; "properties_only")]
    #[test_case(vec![make_foo()], "root/foo" ; "nodes_and_properties")]
    #[test_case(vec![get_test_hierarchy()], "root" ; "select_root")]
    #[fuchsia::test]
    fn test_select_from_hierarchy_tree_selectors(
        expected: Vec<DiagnosticsHierarchy>,
        tree_selector: &str,
    ) {
        let hierarchy = get_test_hierarchy();
        let parsed_selector =
            selectors::parse_selector::<VerboseError>(&format!("*:{tree_selector}"))
                .expect("All test selectors are valid and parsable.");
        let Ok(SelectResult::Nodes(node_vec)) = select_from_hierarchy(&hierarchy, &parsed_selector)
        else {
            panic!("must be nodes");
        };

        let expected = expected.iter().map(Cow::Borrowed).collect::<Vec<_>>();

        assert_eq!(node_vec, expected);
    }

    #[fuchsia::test]
    fn filter_hierarchy_doesnt_return_partial_matches() {
        let hierarchy = DiagnosticsHierarchy::new(
            "root",
            vec![],
            vec![DiagnosticsHierarchy::new("session_started_at", vec![], vec![])],
        );
        let test_selectors = vec!["*:root/session_started_at/0"];
        assert_eq!(parse_selectors_and_filter_hierarchy(hierarchy, test_selectors), None);
    }

    #[fuchsia::test]
    fn test_filter_tree() {
        let test_selectors = vec!["root/foo:11", "root:z", r#"root/bar/zed:13\/\:"#];
        let parsed_test_selectors = test_selectors
            .into_iter()
            .map(|s| {
                selectors::parse_tree_selector::<VerboseError>(s)
                    .expect("All test selectors are valid and parsable.")
            })
            .collect::<Vec<_>>();

        let result =
            filter_tree(get_test_hierarchy(), &parsed_test_selectors).map(|mut hierarchy| {
                hierarchy.sort();
                hierarchy
            });
        assert_eq!(
            result,
            Some(DiagnosticsHierarchy::new(
                "root",
                vec![Property::Int("z".to_string(), -4),],
                vec![
                    DiagnosticsHierarchy::new(
                        "bar",
                        vec![],
                        vec![DiagnosticsHierarchy::new(
                            "zed",
                            vec![Property::Int("13/:".to_string(), -4)],
                            vec![],
                        )],
                    ),
                    DiagnosticsHierarchy::new(
                        "foo",
                        vec![Property::Int("11".to_string(), -4),],
                        vec![],
                    )
                ],
            ))
        );
    }

    #[fuchsia::test]
    fn test_matcher_from_iterator() {
        let matcher = HierarchyMatcher::new(
            ["*:root/foo:11", "*:root:z", r#"*:root/bar/zed:13\/\:"#].into_iter().map(|s| {
                selectors::parse_selector::<VerboseError>(s)
                    .expect("All test selectors are valid and parsable.")
            }),
        )
        .expect("create matcher from iterator of selectors");
        let result = filter_hierarchy(get_test_hierarchy(), &matcher).map(|mut hierarchy| {
            hierarchy.sort();
            hierarchy
        });
        assert_eq!(
            result,
            Some(DiagnosticsHierarchy::new(
                "root",
                vec![Property::Int("z".to_string(), -4),],
                vec![
                    DiagnosticsHierarchy::new(
                        "bar",
                        vec![],
                        vec![DiagnosticsHierarchy::new(
                            "zed",
                            vec![Property::Int("13/:".to_string(), -4)],
                            vec![],
                        )],
                    ),
                    DiagnosticsHierarchy::new(
                        "foo",
                        vec![Property::Int("11".to_string(), -4),],
                        vec![],
                    )
                ],
            ))
        );
    }

    #[fuchsia::test]
    fn hierarchy_matcher_matches_property() {
        let matcher = HierarchyMatcher::from_selector_strs([
            "*:root:z",
            "*:root/foo:11",
            r#"*:root/bar/zed:13\/\:"#,
            "*:root/sub",
            "*:root/w*:p*",
        ])
        .expect("create matcher");
        assert!(HierarchyMatcher::from_selector_strs(["invalid"]).is_err());

        // Exact property matches.
        assert!(matcher.matches_property(&["root"], "z"));
        assert!(matcher.matches_property(&["root", "foo"], "11"));
        assert!(matcher.matches_property(&["root", "bar", "zed"], "13/:"));

        // Wildcard node and property matches.
        assert!(matcher.matches_property(&["root", "wild"], "prop_1"));
        assert!(!matcher.matches_property(&["root", "wild"], "other"));

        // Subtree selector matches properties on the node and at arbitrary depth below it.
        assert!(matcher.matches_property(&["root", "sub"], "direct_prop"));
        assert!(matcher.matches_property(&["root", "sub", "nested", "deep"], "any_prop"));

        // Non-matching paths, empty path, and partial node paths without subtree/property matchers.
        assert!(!matcher.matches_property(&[] as &[&str], "z"));
        assert!(!matcher.matches_property(&["not_root"], "z"));
        assert!(!matcher.matches_property(&["root"], "unselected"));
        assert!(!matcher.matches_property(&["root", "bar"], "13/:"));
        assert!(!matcher.matches_property(&["root", "unknown"], "11"));
        assert!(!HierarchyMatcher::default().matches_property(&["root"], "z"));
    }

    #[fuchsia::test]
    fn hierarchy_matcher_matches_property_agrees_with_filter_hierarchy() {
        let matcher = HierarchyMatcher::from_selector_strs([
            "*:root/foo:11",
            "*:root:z",
            r#"*:root/bar/zed:13\/\:"#,
            "*:root/foo/zed",
        ])
        .expect("create matcher");

        let unfiltered = get_test_hierarchy();
        let filtered =
            filter_hierarchy(unfiltered.clone(), &matcher).expect("filtered hierarchy exists");

        for (node_path, maybe_prop) in unfiltered.property_iter() {
            let Some(prop) = maybe_prop else {
                continue;
            };
            let matched = matcher.matches_property(&node_path, prop.name());
            let in_filtered = filtered
                .property_iter()
                .any(|(f_path, f_prop)| f_path == node_path && f_prop == Some(prop));
            assert_eq!(
                matched,
                in_filtered,
                "mismatch at {:?}:{}: matches_property={matched}, in_filtered={in_filtered}",
                node_path,
                prop.name()
            );
        }
    }
}
