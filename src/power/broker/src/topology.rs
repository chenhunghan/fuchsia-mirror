// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Manages the Power Element Topology, keeping track of element dependencies.
use crate::inspect::{
    AddElementInspectWriter, DependencyData, EagerInspectWriter, ElementData, InspectAddDependency,
    TopologyInspect,
};
use fidl_fuchsia_power_broker::{self as fpb};
use fuchsia_inspect as inspect;
use fuchsia_inspect_contrib::graph as igraph;
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::{fmt, ops};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexedPowerLevel {
    pub level: fpb::PowerLevel,
    pub index: usize,
}

impl IndexedPowerLevel {
    pub const MIN: IndexedPowerLevel = IndexedPowerLevel { level: fpb::PowerLevel::MIN, index: 0 };

    #[allow(dead_code)]
    #[cfg(test)]
    pub const MAX: IndexedPowerLevel = Self { level: fpb::PowerLevel::MAX, index: usize::MAX };

    #[cfg(test)]
    pub const fn from_same_level_and_index(level_and_index: u8) -> Self {
        Self { level: level_and_index, index: level_and_index as usize }
    }
}

impl fmt::Display for IndexedPowerLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.level)
    }
}

impl std::cmp::PartialOrd for IndexedPowerLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.index.partial_cmp(&other.index)
    }
}

impl std::cmp::Ord for IndexedPowerLevel {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index.cmp(&other.index)
    }
}

/// A IndexedPowerLevel satisfies a required IndexedPowerLevel if it is
/// greater than or equal to it on the same scale.
pub trait SatisfyPowerLevel {
    fn satisfies(&self, required: IndexedPowerLevel) -> bool;
}

impl SatisfyPowerLevel for IndexedPowerLevel {
    fn satisfies(&self, required: IndexedPowerLevel) -> bool {
        self >= &required
    }
}

impl SatisfyPowerLevel for Option<IndexedPowerLevel> {
    fn satisfies(&self, required: IndexedPowerLevel) -> bool {
        self.is_some() && self.unwrap().satisfies(required)
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)]
pub struct ElementID(u32);

impl ElementID {
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    fn generate() -> Self {
        Self(rand::random::<u32>())
    }
}

impl fmt::Display for ElementID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ops::Deref for ElementID {
    type Target = u32;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct ElementLevel {
    pub element_id: ElementID,
    pub level: IndexedPowerLevel,
}

impl fmt::Display for ElementLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.element_id, self.level)
    }
}

impl ElementLevel {
    pub fn satisfies(&self, required: &ElementLevel) -> bool {
        self.element_id == required.element_id && self.level.satisfies(required.level)
    }
}

/// Power dependency from one element's IndexedPowerLevel to another.
/// The Element and IndexedPowerLevel specified by `dependent` depends on
/// the Element and IndexedPowerLevel specified by `requires`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)]
pub struct Dependency {
    pub dependent: ElementLevel,
    pub requires: ElementLevel,
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Dep{{{}->{}}}", self.dependent, self.requires)
    }
}

#[derive(Clone, Debug)]
pub struct Element {
    pub(crate) id: ElementID,
    pub(crate) name: String,
    pub(crate) valid_levels: Vec<IndexedPowerLevel>,
    pub(crate) synthetic: bool,
    pub(crate) inspect_vertex: Option<Rc<RefCell<igraph::Vertex<ElementData>>>>,
    pub(crate) inspect_edges: Rc<RefCell<HashMap<ElementID, igraph::Edge<DependencyData>>>>,
}

impl Element {
    fn new(
        id: ElementID,
        name: String,
        mut valid_levels: Vec<IndexedPowerLevel>,
        synthetic: bool,
    ) -> Self {
        valid_levels.sort();
        Self {
            id,
            name,
            valid_levels,
            synthetic,
            inspect_vertex: None,
            inspect_edges: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OnRequiredElementRemoval {
    RemoveWithRequiredElement,
    MakeUnsatisfiable,
}

impl OnRequiredElementRemoval {
    pub fn from_level_dependency(dep: fpb::LevelDependency) -> Self {
        match dep.remove_with_required_element {
            Some(true) => Self::RemoveWithRequiredElement,
            _ => Self::MakeUnsatisfiable,
        }
    }
}

#[derive(Debug)]
pub enum AddElementError {
    Invalid,
    NotAuthorized,
}

impl Into<fpb::AddElementError> for AddElementError {
    fn into(self) -> fpb::AddElementError {
        match self {
            AddElementError::Invalid => fpb::AddElementError::Invalid,
            AddElementError::NotAuthorized => fpb::AddElementError::NotAuthorized,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ModifyDependencyError {
    AlreadyExists,
    Invalid,
    NotAuthorized,
    // Suppress `dead_code`, callers may want to know what ElementID was not found, but currently
    // no one makes use of this or prints it out.
    #[allow(dead_code)]
    NotFound(ElementID),
}

impl Into<fpb::ModifyDependencyError> for ModifyDependencyError {
    fn into(self) -> fpb::ModifyDependencyError {
        match self {
            ModifyDependencyError::AlreadyExists => fpb::ModifyDependencyError::AlreadyExists,
            ModifyDependencyError::Invalid => fpb::ModifyDependencyError::Invalid,
            ModifyDependencyError::NotAuthorized => fpb::ModifyDependencyError::NotAuthorized,
            ModifyDependencyError::NotFound(_) => fpb::ModifyDependencyError::NotFound,
        }
    }
}

#[derive(Debug)]
pub struct Topology {
    pub(crate) elements: HashMap<ElementID, Element>,
    dependencies: HashMap<ElementLevel, Vec<ElementLevel>>,
    deps_by_required_element: FxHashMap<ElementID, FxHashSet<Dependency>>,
    deps_by_dependent_element: FxHashMap<ElementID, FxHashSet<Dependency>>,
    removable_dependencies: FxHashSet<Dependency>,
    unsatisfiable_element_id: ElementID,
    inspect: TopologyInspect,
}

impl Topology {
    const TOPOLOGY_UNSATISFIABLE_ELEMENT: &'static str = "TOPOLOGY_UNSATISFIABLE_ELEMENT";
    const TOPOLOGY_UNSATISFIABLE_ELEMENT_POWER_LEVELS: [fpb::PowerLevel; 2] =
        [fpb::PowerLevel::MIN, fpb::PowerLevel::MAX];

    pub fn new(parent_inspect_node: &inspect::Node, inspect_max_event: usize) -> Self {
        let mut topology = Topology {
            elements: HashMap::new(),
            dependencies: HashMap::new(),
            deps_by_required_element: FxHashMap::default(),
            deps_by_dependent_element: FxHashMap::default(),
            removable_dependencies: FxHashSet::default(),
            unsatisfiable_element_id: ElementID::new(0),
            inspect: TopologyInspect::new(
                parent_inspect_node.create_child("topology"),
                inspect_max_event,
            ),
        };
        topology.unsatisfiable_element_id = topology
            .add_element(
                Self::TOPOLOGY_UNSATISFIABLE_ELEMENT,
                &Self::TOPOLOGY_UNSATISFIABLE_ELEMENT_POWER_LEVELS,
            )
            .ok()
            .expect("Failed to add unsatisfiable element");
        AddElementInspectWriter::new(topology.unsatisfiable_element_id).commit(&mut topology);
        topology
    }

    #[cfg(test)]
    pub fn get_unsatisfiable_element(&self) -> Element {
        self.elements.get(&self.unsatisfiable_element_id).unwrap().clone()
    }

    #[cfg(test)]
    pub fn get_unsatisfiable_element_name(&self) -> String {
        Self::TOPOLOGY_UNSATISFIABLE_ELEMENT.to_string().clone()
    }

    #[cfg(test)]
    pub fn get_unsatisfiable_element_id(&self) -> ElementID {
        self.unsatisfiable_element_id
    }

    #[cfg(test)]
    pub fn get_unsatisfiable_element_levels(&self) -> Vec<u64> {
        Self::TOPOLOGY_UNSATISFIABLE_ELEMENT_POWER_LEVELS
            .iter()
            .map(|&v| v as u64)
            .collect::<Vec<_>>()
            .clone()
    }

    pub fn inspect(&self) -> &TopologyInspect {
        &self.inspect
    }

    pub fn get_element(&self, id: &ElementID) -> Option<&Element> {
        self.elements.get(id)
    }

    pub fn get_element_mut(&mut self, id: &ElementID) -> Option<&mut Element> {
        self.elements.get_mut(id)
    }

    pub fn add_element(
        &mut self,
        name: &str,
        valid_levels: &[fpb::PowerLevel],
    ) -> Result<ElementID, AddElementError> {
        self.add_element_internal(name, valid_levels, false)
    }

    pub fn add_synthetic_element(
        &mut self,
        name: &str,
        valid_levels: &[fpb::PowerLevel],
    ) -> Result<ElementID, AddElementError> {
        self.add_element_internal(name, valid_levels, true)
    }

    fn add_element_internal(
        &mut self,
        name: &str,
        valid_levels: &[fpb::PowerLevel],
        synthetic: bool,
    ) -> Result<ElementID, AddElementError> {
        let id = {
            loop {
                let element_id = ElementID::generate();
                if !self.elements.contains_key(&element_id) {
                    break element_id;
                }
            }
        };
        let valid_levels = valid_levels
            .iter()
            .enumerate()
            .map(|(index, level)| IndexedPowerLevel { level: *level, index })
            .collect();
        self.elements.insert(id, Element::new(id, name.into(), valid_levels, synthetic));
        Ok(id)
    }

    #[cfg(test)]
    pub fn element_exists(&self, element_id: ElementID) -> bool {
        self.elements.contains_key(&element_id)
    }

    #[cfg(test)]
    pub fn element_is_synthetic(&self, element_id: ElementID) -> bool {
        self.elements.get(&element_id).and_then(|x| Some(x.synthetic)).unwrap_or(false)
    }

    pub fn element_name(&self, element_id: ElementID) -> Cow<'_, str> {
        Cow::from(
            self.elements.get(&element_id).and_then(|e| Some(e.name.as_str())).unwrap_or_default(),
        )
    }

    pub fn remove_element(&mut self, element_id: ElementID) -> Option<Element> {
        if self.unsatisfiable_element_id != element_id {
            self.update_dependencies_for_removed_element(element_id);
            self.elements.remove(&element_id)
        } else {
            None
        }
    }

    pub fn minimum_level(&self, element_id: ElementID) -> IndexedPowerLevel {
        let Some(elem) = self.elements.get(&element_id) else {
            return IndexedPowerLevel::MIN;
        };
        match elem.valid_levels.first().copied() {
            Some(level) => level,
            None => IndexedPowerLevel::MIN,
        }
    }

    pub fn is_valid_level(&self, element_id: ElementID, level: IndexedPowerLevel) -> bool {
        let Some(elem) = self.elements.get(&element_id) else {
            return false;
        };
        elem.valid_levels.contains(&level)
    }

    pub fn get_level_index(
        &self,
        element_id: ElementID,
        level: &fpb::PowerLevel,
    ) -> Option<&IndexedPowerLevel> {
        let Some(elem) = self.elements.get(&element_id) else {
            return Some(&IndexedPowerLevel::MIN);
        };
        elem.valid_levels.iter().find(|l| &l.level == level)
    }

    fn decrement_element_level_index(
        &self,
        element_id: ElementID,
        level: &IndexedPowerLevel,
    ) -> IndexedPowerLevel {
        if level.index < 1 {
            return IndexedPowerLevel::MIN;
        }
        let Some(elem) = self.elements.get(&element_id) else {
            return IndexedPowerLevel::MIN;
        };
        return elem.valid_levels[level.index - 1];
    }

    /// Gets direct dependencies for the given Element and PowerLevel.
    pub fn direct_dependencies<'a>(
        &'a self,
        element_level: &'a ElementLevel,
    ) -> impl Iterator<Item = Dependency> + 'a {
        self.dependencies
            .get(&element_level)
            .into_iter()
            .flat_map(|required_levels| required_levels.iter())
            .map(|required| Dependency {
                dependent: element_level.clone(),
                requires: required.clone(),
            })
    }

    /// Returns an iterator over all dependencies where `element_id` is the required element.
    pub fn dependencies_for_required_element(
        &self,
        element_id: ElementID,
    ) -> impl Iterator<Item = &Dependency> {
        self.deps_by_required_element.get(&element_id).into_iter().flat_map(|deps| deps.iter())
    }

    /// Returns an iterator over all dependencies where `element_id` is the dependent element.
    pub fn dependencies_for_dependent_element(
        &self,
        element_id: ElementID,
    ) -> impl Iterator<Item = &Dependency> {
        self.deps_by_dependent_element.get(&element_id).into_iter().flat_map(|deps| deps.iter())
    }

    /// Gets direct and transitive dependencies for the given Element and
    /// IndexedPowerLevel.
    pub fn all_direct_and_indirect_dependencies(
        &self,
        element_level: &ElementLevel,
    ) -> Vec<Dependency> {
        // We need to inspect the required level of every dependency encountered for any transitive
        // dependencies.
        let mut dependencies = FxHashSet::<Dependency>::default();
        let mut visited = FxHashSet::<ElementLevel>::default();
        let mut element_levels_to_inspect = vec![element_level.clone()];
        while let Some(element_level) = element_levels_to_inspect.pop() {
            if visited.contains(&element_level) {
                continue;
            }
            visited.insert(element_level.clone());

            if element_level.level != self.minimum_level(element_level.element_id) {
                let mut lower_element_level = element_level.clone();
                lower_element_level.level = self
                    .decrement_element_level_index(element_level.element_id, &element_level.level);
                element_levels_to_inspect.push(lower_element_level);
            }
            for dep in self.direct_dependencies(&element_level) {
                element_levels_to_inspect.push(dep.requires.clone());
                dependencies.insert(dep);
            }
        }
        dependencies.into_iter().collect()
    }

    /// Updates dependencies when an element is removed.
    /// Non-removable dependencies are replaced with a dependency on the unsatisfiable element.
    /// Removable dependencies are simply removed, along with all other dependencies where the
    /// removed element is the dependent element.
    fn update_dependencies_for_removed_element(&mut self, removed_element_id: ElementID) {
        // For each dependency that is not removable, we must first
        // replace the dependency with one on the unsatisfiable element.
        let dependencies_on_removed_element: Vec<Dependency> =
            self.dependencies_for_required_element(removed_element_id).cloned().collect();
        for dep in dependencies_on_removed_element {
            if !self.removable_dependencies.contains(&dep) {
                match self.add_dependency(
                    &Dependency {
                        dependent: dep.dependent.clone(),
                        requires: ElementLevel {
                            element_id: self.unsatisfiable_element_id,
                            level: IndexedPowerLevel { level: fpb::PowerLevel::MAX, index: 1 },
                        },
                    },
                    OnRequiredElementRemoval::MakeUnsatisfiable,
                    &mut EagerInspectWriter,
                ) {
                    Ok(_) | Err(ModifyDependencyError::AlreadyExists) => {
                        // This is fine, there could be multiple removed elements.
                    }
                    Err(e) => {
                        panic!(
                            "failed to replace dependency with unsatisfiable dependency: {:?}",
                            e
                        )
                    }
                }
            }
            self.remove_dependency(&dep).expect("failed to remove dependency");
        }
        // Remove all dependencies where this element is the dependent element.
        let dependencies_from_removed_element: Vec<Dependency> =
            self.dependencies_for_dependent_element(removed_element_id).cloned().collect();
        for dep in dependencies_from_removed_element {
            self.remove_dependency(&dep).expect("failed to remove dependency");
        }
        self.dependencies.retain(|key, _| key.element_id != removed_element_id);
    }

    /// Checks that a dependency is valid. Returns ModifyDependencyError if not.
    fn check_valid_dependency(&self, dep: &Dependency) -> Result<(), ModifyDependencyError> {
        if dep.dependent.element_id == dep.requires.element_id {
            return Err(ModifyDependencyError::Invalid);
        }
        if !self.elements.contains_key(&dep.dependent.element_id) {
            return Err(ModifyDependencyError::NotFound(dep.dependent.element_id));
        }
        if !self.elements.contains_key(&dep.requires.element_id) {
            return Err(ModifyDependencyError::NotFound(dep.requires.element_id));
        }
        if !self.is_valid_level(dep.dependent.element_id, dep.dependent.level) {
            return Err(ModifyDependencyError::Invalid);
        }
        if !self.is_valid_level(dep.requires.element_id, dep.requires.level) {
            return Err(ModifyDependencyError::Invalid);
        }
        if self.unsatisfiable_element_id == dep.dependent.element_id {
            return Err(ModifyDependencyError::Invalid);
        }
        Ok(())
    }

    /// Adds a dependency to the Topology.
    pub fn add_dependency<I>(
        &mut self,
        dep: &Dependency,
        on_required_element_removal: OnRequiredElementRemoval,
        inspect_writer: &mut I,
    ) -> Result<(), ModifyDependencyError>
    where
        I: InspectAddDependency,
    {
        self.check_valid_dependency(dep)?;
        let required_levels = self.dependencies.entry(dep.dependent.clone()).or_insert(Vec::new());
        if required_levels.contains(&dep.requires) {
            return Err(ModifyDependencyError::AlreadyExists);
        }
        required_levels.push(dep.requires.clone());
        self.deps_by_required_element
            .entry(dep.requires.element_id)
            .or_default()
            .insert(dep.clone());
        self.deps_by_dependent_element
            .entry(dep.dependent.element_id)
            .or_default()
            .insert(dep.clone());
        let remove_with_required_element =
            on_required_element_removal == OnRequiredElementRemoval::RemoveWithRequiredElement;
        if remove_with_required_element {
            self.mark_dependency_removable(dep.clone());
        }
        inspect_writer.add_dependency(&self, dep, on_required_element_removal);
        Ok(())
    }

    /// Removes a dependency from the Topology.
    pub fn remove_dependency(&mut self, dep: &Dependency) -> Result<(), ModifyDependencyError> {
        if !self.elements.contains_key(&dep.dependent.element_id) {
            return Err(ModifyDependencyError::NotFound(dep.dependent.element_id));
        }
        if !self.elements.contains_key(&dep.requires.element_id) {
            return Err(ModifyDependencyError::NotFound(dep.requires.element_id));
        }
        let required_levels = self.dependencies.entry(dep.dependent.clone()).or_insert(Vec::new());
        if !required_levels.contains(&dep.requires) {
            return Err(ModifyDependencyError::NotFound(dep.requires.element_id));
        }
        required_levels.retain(|el| el != &dep.requires);
        if let Some(deps) = self.deps_by_required_element.get_mut(&dep.requires.element_id) {
            deps.remove(dep);
            if deps.is_empty() {
                self.deps_by_required_element.remove(&dep.requires.element_id);
            }
        }
        if let Some(deps) = self.deps_by_dependent_element.get_mut(&dep.dependent.element_id) {
            deps.remove(dep);
            if deps.is_empty() {
                self.deps_by_dependent_element.remove(&dep.dependent.element_id);
            }
        }
        self.removable_dependencies.remove(dep);
        self.inspect.on_remove_dependency(&self.elements, dep);
        Ok(())
    }

    /// Marks a dependency to be automatically removed when its required element is removed.
    pub fn mark_dependency_removable(&mut self, dep: Dependency) {
        self.removable_dependencies.insert(dep);
    }

    /// Returns all removable dependencies that require the given element.
    pub fn get_removable_dependencies_for_required_element(
        &self,
        element_id: ElementID,
    ) -> Vec<Dependency> {
        self.removable_dependencies
            .iter()
            .filter(|dep| dep.requires.element_id == element_id)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::InspectUpdateLevel;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use power_broker_client::BINARY_POWER_LEVELS;

    const BINARY_POWER_LEVEL_ON: IndexedPowerLevel = IndexedPowerLevel { level: 1, index: 1 };

    const ONE: IndexedPowerLevel = IndexedPowerLevel::from_same_level_and_index(1);
    const TWO: IndexedPowerLevel = IndexedPowerLevel::from_same_level_and_index(2);

    impl Topology {
        fn add_element_with_inspect(
            &mut self,
            name: &str,
            valid_levels: Vec<fpb::PowerLevel>,
            current_level: fpb::PowerLevel,
            required_level: fpb::PowerLevel,
        ) -> Result<ElementID, AddElementError> {
            let element_id = self.add_element(name, &valid_levels)?;
            let mut writer = AddElementInspectWriter::new(element_id);
            writer.update_current_level(self, element_id, current_level).update_required_level(
                self,
                element_id,
                required_level,
            );
            writer.commit(self);
            Ok(element_id)
        }
    }

    #[fuchsia::test]
    async fn test_add_remove_elements() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);
        let water = t
            .add_element_with_inspect(
                "Water",
                BINARY_POWER_LEVELS.to_vec(),
                fpb::BinaryPowerLevel::On as u8,
                fpb::BinaryPowerLevel::On as u8,
            )
            .expect("add_element failed");
        let earth = t
            .add_element_with_inspect(
                "Earth",
                BINARY_POWER_LEVELS.to_vec(),
                fpb::BinaryPowerLevel::Off as u8,
                fpb::BinaryPowerLevel::On as u8,
            )
            .expect("add_element failed");
        let fire = t
            .add_element_with_inspect(
                "Fire",
                BINARY_POWER_LEVELS.to_vec(),
                fpb::BinaryPowerLevel::On as u8,
                fpb::BinaryPowerLevel::Off as u8,
            )
            .expect("add_element failed");
        let air = t
            .add_element_with_inspect(
                "Air",
                BINARY_POWER_LEVELS.to_vec(),
                fpb::BinaryPowerLevel::Off as u8,
                fpb::BinaryPowerLevel::Off as u8,
            )
            .expect("add_element failed");
        let v01: Vec<u64> = BINARY_POWER_LEVELS.iter().map(|&v| v as u64).collect();
        assert_data_tree!(inspect, root: {
            topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        t.get_unsatisfiable_element_id().to_string() => {
                            meta: {
                                name: t.get_unsatisfiable_element().name,
                                valid_levels: t.get_unsatisfiable_element_levels(),
                                current_level: "unset",
                                required_level: "unset",
                                leases: {}
                            },
                            relationships: {},
                        },
                        water.to_string() => {
                            meta: {
                                name: "Water",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 1u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        earth.to_string() => {
                            meta: {
                                name: "Earth",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 1u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        fire.to_string() => {
                            meta: {
                                name: "Fire",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        air.to_string() => {
                            meta: {
                                name: "Air",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
        }}}});

        t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: water.clone(), level: BINARY_POWER_LEVEL_ON },
                requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        assert_data_tree!(inspect, root: {
           topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        t.get_unsatisfiable_element().id.to_string() => {
                            meta: {
                                name: t.get_unsatisfiable_element().name,
                                valid_levels: t.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {}
                            },
                            relationships: {}
                        },
                        water.to_string() => {
                            meta: {
                                name: "Water",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 1u64,
                                leases: {}
                            },
                            relationships: {
                                earth.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {
                                        "1": {
                                            required_level: 1u64,
                                        }
                                    },
                                },
                            },
                        },
                        earth.to_string() => {
                            meta: {
                                name: "Earth",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 1u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        fire.to_string() => {
                            meta: {
                                name: "Fire",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        air.to_string() => {
                            meta: {
                                name: "Air",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
        }}}});

        let extra_add_dep_res = t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: water.clone(), level: BINARY_POWER_LEVEL_ON },
                requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        );
        assert!(matches!(extra_add_dep_res, Err(ModifyDependencyError::AlreadyExists { .. })));

        t.remove_dependency(&Dependency {
            dependent: ElementLevel { element_id: water.clone(), level: BINARY_POWER_LEVEL_ON },
            requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
        })
        .expect("remove_dependency failed");
        assert_data_tree!(inspect, root: {
           topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        t.get_unsatisfiable_element().id.to_string() => {
                            meta: {
                                name: t.get_unsatisfiable_element().name,
                                valid_levels: t.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {}
                            },
                            relationships: {}},
                        water.to_string() => {
                            meta: {
                                name: "Water",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 1u64,
                                leases: {}
                            },
                            relationships: {
                                earth.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {},
                                },
                            },
                        },
                        earth.to_string() => {
                            meta: {
                                name: "Earth",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 1u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        fire.to_string() => {
                            meta: {
                                name: "Fire",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        air.to_string() => {
                            meta: {
                                name: "Air",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
        }}}});

        let extra_remove_dep_res = t.remove_dependency(&Dependency {
            dependent: ElementLevel { element_id: water.clone(), level: BINARY_POWER_LEVEL_ON },
            requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
        });
        assert!(matches!(extra_remove_dep_res, Err(ModifyDependencyError::NotFound { .. })));

        assert_eq!(t.element_exists(fire), true);
        t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: fire.clone(), level: BINARY_POWER_LEVEL_ON },
                requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        t.remove_element(fire);
        assert_eq!(t.element_exists(fire), false);
        let removed_element_dep_res = t.remove_dependency(&Dependency {
            dependent: ElementLevel { element_id: fire.clone(), level: BINARY_POWER_LEVEL_ON },
            requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
        });
        assert!(matches!(removed_element_dep_res, Err(ModifyDependencyError::NotFound { .. })));

        assert_eq!(t.element_exists(air), true);
        t.remove_element(air);
        assert_eq!(t.element_exists(air), false);

        assert_data_tree!(inspect, root: {
           topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        t.get_unsatisfiable_element().id.to_string() => {
                            meta: {
                                name: t.get_unsatisfiable_element().name,
                                valid_levels: t.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {},
                            },
                            relationships: {}},
                        water.to_string() => {
                            meta: {
                                name: "Water",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 1u64,
                                leases: {},
                            },
                            relationships: {
                                earth.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {},
                                },
                            },
                        },
                        earth.to_string() => {
                            meta: {
                                name: "Earth",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 1u64,
                                leases: {},
                            },
                            relationships: {},
                        },
        }}}});

        let element_not_found_res = t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: air.clone(), level: BINARY_POWER_LEVEL_ON },
                requires: ElementLevel { element_id: water.clone(), level: BINARY_POWER_LEVEL_ON },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        );
        assert!(matches!(element_not_found_res, Err(ModifyDependencyError::NotFound { .. })));

        let req_element_not_found_res = t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
                requires: ElementLevel { element_id: fire.clone(), level: BINARY_POWER_LEVEL_ON },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        );
        assert!(matches!(req_element_not_found_res, Err(ModifyDependencyError::NotFound { .. })));

        assert_data_tree!(inspect, root: {
           topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        t.get_unsatisfiable_element().id.to_string() => {
                            meta: {
                                name: t.get_unsatisfiable_element().name,
                                valid_levels: t.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {},
                            },
                            relationships: {}},
                        water.to_string() => {
                            meta: {
                                name: "Water",
                                valid_levels: v01.clone(),
                                current_level: 1u64,
                                required_level: 1u64,
                                leases: {},
                            },
                            relationships: {
                                earth.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {},
                                },
                            },
                        },
                        earth.to_string() => {
                            meta: {
                                name: "Earth",
                                valid_levels: v01.clone(),
                                current_level: 0u64,
                                required_level: 1u64,
                                leases: {},
                            },
                            relationships: {},
                        },
        }}}});

        t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: water.clone(), level: BINARY_POWER_LEVEL_ON },
                requires: ElementLevel { element_id: earth.clone(), level: BINARY_POWER_LEVEL_ON },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        assert_data_tree!(inspect, root: { topology: {
            "fuchsia.inspect.Graph": { "topology": {
            t.get_unsatisfiable_element().id.to_string() => {
                meta: {
                    name: t.get_unsatisfiable_element().name,
                    valid_levels: t.get_unsatisfiable_element_levels(),
                    required_level: "unset",
                    current_level: "unset",
                    leases: {},
                },
                relationships: {}},
            water.to_string() => {
                meta: {
                    name: "Water",
                    valid_levels: v01.clone(),
                    current_level: 1u64,
                    required_level: 1u64,
                    leases: {},
                },
                relationships: {
                    earth.to_string() => {
                        edge_id: AnyProperty,
                        meta: {
                            "1": {
                                required_level: 1u64,
                            }
                        }
                    },
                },
            },
            earth.to_string() => {
                meta: {
                    name: "Earth",
                    valid_levels: v01.clone(),
                    current_level: 0u64,
                    required_level: 1u64,
                    leases: {},
                },
                relationships: {},
            },
        }}}});

        t.remove_element(earth);
        assert_eq!(t.element_exists(earth), false);
        assert_data_tree!(inspect, root: { topology: { "fuchsia.inspect.Graph": { "topology": {
            t.get_unsatisfiable_element().id.to_string() => {
                meta: {
                    name: t.get_unsatisfiable_element().name,
                    valid_levels: t.get_unsatisfiable_element_levels(),
                    required_level: "unset",
                    current_level: "unset",
                    leases: {},
                },
                relationships: {}
            },
            water.to_string() => {
                meta: {
                    name: "Water",
                    valid_levels: v01.clone(),
                    current_level: 1u64,
                    required_level: 1u64,
                    leases: {},
                },
                relationships: {
                    t.get_unsatisfiable_element().id.to_string() => {
                        edge_id: AnyProperty,
                        meta: {
                            "1": {
                                required_level: fpb::PowerLevel::MAX as u64,
                            }
                        }
                    },
                },
            },
        }}}});

        let synthetic_element = t
            .add_synthetic_element("Synthetic", &BINARY_POWER_LEVELS)
            .expect("add_synthetic_element failed");
        assert_eq!(t.element_exists(synthetic_element), true);
        assert_eq!(t.element_is_synthetic(synthetic_element), true);
    }

    #[fuchsia::test]
    async fn test_add_remove_direct_deps() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);

        let v012_u8: Vec<u8> = vec![0, 1, 2];
        let v012: Vec<u64> = v012_u8.iter().map(|&v| v as u64).collect();

        let a = t.add_element_with_inspect("A", v012_u8.clone(), 0, 0).expect("add_element failed");
        let b = t.add_element_with_inspect("B", v012_u8.clone(), 0, 0).expect("add_element failed");
        let c = t.add_element_with_inspect("C", v012_u8.clone(), 0, 0).expect("add_element failed");
        let d = t.add_element_with_inspect("D", v012_u8.clone(), 0, 0).expect("add_element failed");
        // A <- B <- C -> D
        let ba = Dependency {
            dependent: ElementLevel { element_id: b.clone(), level: ONE },
            requires: ElementLevel { element_id: a.clone(), level: ONE },
        };
        t.add_dependency(&ba, OnRequiredElementRemoval::MakeUnsatisfiable, &mut EagerInspectWriter)
            .expect("add_dependency failed");
        let cb = Dependency {
            dependent: ElementLevel { element_id: c.clone(), level: ONE },
            requires: ElementLevel { element_id: b.clone(), level: ONE },
        };
        t.add_dependency(&cb, OnRequiredElementRemoval::MakeUnsatisfiable, &mut EagerInspectWriter)
            .expect("add_dependency failed");
        let cd = Dependency {
            dependent: ElementLevel { element_id: c.clone(), level: ONE },
            requires: ElementLevel { element_id: d.clone(), level: ONE },
        };
        t.add_dependency(&cd, OnRequiredElementRemoval::MakeUnsatisfiable, &mut EagerInspectWriter)
            .expect("add_dependency failed");
        let cd2 = Dependency {
            dependent: ElementLevel { element_id: c.clone(), level: TWO },
            requires: ElementLevel { element_id: d.clone(), level: TWO },
        };
        t.add_dependency(
            &cd2,
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        assert_data_tree!(inspect, root: {
            topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        t.get_unsatisfiable_element().id.to_string() => {
                            meta: {
                                name: t.get_unsatisfiable_element().name,
                                valid_levels: t.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {}
                            },
                            relationships: {}},
                        a.to_string() => {
                            meta: {
                                name: "A",
                                valid_levels: v012.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        b.to_string() => {
                            meta: {
                                name: "B",
                                valid_levels: v012.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {
                                a.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {
                                        "1": {
                                            required_level: 1u64,
                                        }
                                    }
                                },
                            },
                        },
                        c.to_string() => {
                            meta: {
                                name: "C",
                                valid_levels: v012.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {
                                b.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {
                                        "1": {
                                            required_level: 1u64,
                                        }
                                    }
                                },
                                d.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {
                                        "1": {
                                            required_level: 1u64,
                                        },
                                        "2": {
                                            required_level: 2u64,
                                        }
                                    }
                                },
                            },
                        },
                        d.to_string() => {
                            meta: {
                                name: "D",
                                valid_levels: v012.clone(),
                                current_level: 0u64,
                                required_level: 0u64,
                                leases: {}
                            },
                            relationships: {},
                        },
        }}}});

        let mut a_deps = t
            .direct_dependencies(&ElementLevel { element_id: a.clone(), level: ONE })
            .collect::<Vec<_>>();
        a_deps.sort();
        assert_eq!(a_deps, []);

        let mut b_deps = t
            .direct_dependencies(&ElementLevel { element_id: b.clone(), level: ONE })
            .collect::<Vec<_>>();
        b_deps.sort();
        assert_eq!(b_deps, [ba]);

        let mut c_deps = t
            .direct_dependencies(&ElementLevel { element_id: c.clone(), level: ONE })
            .collect::<Vec<_>>();
        let mut want_c_deps = [cb, cd];
        c_deps.sort();
        want_c_deps.sort();
        assert_eq!(c_deps, want_c_deps);
    }

    #[fuchsia::test]
    fn test_all_direct_and_indirect_dependencies() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);

        let a =
            t.add_element_with_inspect("A", vec![0, 1, 2, 3], 0, 0).expect("add_element failed");
        let b = t.add_element_with_inspect("B", vec![0, 1, 5], 0, 0).expect("add_element failed");
        let c = t.add_element_with_inspect("C", vec![0, 1], 0, 0).expect("add_element failed");
        let d = t.add_element_with_inspect("D", vec![0, 1, 3], 0, 0).expect("add_element failed");

        // C has direct dependencies on B and D.
        // D has a direct dependency on A.
        //
        // C has an *implicit* transitive dependency on A[1] (through D[1]).
        //
        // A    B    C    D
        // 1 <=========== 1
        // 3    5 <= 1 => 3

        let c1_b5 = Dependency {
            dependent: ElementLevel { element_id: c.clone(), level: ONE },
            requires: ElementLevel {
                element_id: b.clone(),
                level: IndexedPowerLevel { level: 5, index: 2 },
            },
        };
        t.add_dependency(
            &c1_b5,
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        let c1_d3 = Dependency {
            dependent: ElementLevel { element_id: c.clone(), level: ONE },
            requires: ElementLevel {
                element_id: d.clone(),
                level: IndexedPowerLevel { level: 3, index: 2 },
            },
        };
        t.add_dependency(
            &c1_d3,
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        let d1_a1 = Dependency {
            dependent: ElementLevel { element_id: d.clone(), level: ONE },
            requires: ElementLevel { element_id: a.clone(), level: ONE },
        };
        t.add_dependency(
            &d1_a1,
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");

        let a_deps = t.all_direct_and_indirect_dependencies(&ElementLevel {
            element_id: a.clone(),
            level: ONE,
        });
        assert_eq!(a_deps, []);

        let b1_deps = t.all_direct_and_indirect_dependencies(&ElementLevel {
            element_id: b.clone(),
            level: ONE,
        });
        assert_eq!(b1_deps, []);

        let b5_deps = t.all_direct_and_indirect_dependencies(&ElementLevel {
            element_id: b.clone(),
            level: IndexedPowerLevel { level: 5, index: 2 },
        });
        assert_eq!(b5_deps, []);

        let mut c_deps = t.all_direct_and_indirect_dependencies(&ElementLevel {
            element_id: c.clone(),
            level: ONE,
        });
        let mut want_c_deps = [c1_b5.clone(), c1_d3.clone(), d1_a1.clone()];
        c_deps.sort();
        want_c_deps.sort();
        assert_eq!(c_deps, want_c_deps);

        t.remove_dependency(&c1_d3).expect("remove_direct_dep failed");
        let c_deps = t.all_direct_and_indirect_dependencies(&ElementLevel {
            element_id: c.clone(),
            level: ONE,
        });
        assert_eq!(c_deps, [c1_b5.clone()]);
    }

    #[fuchsia::test]
    fn test_invalidate_multiple_dependencies() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);

        let a = t.add_element_with_inspect("A", vec![0, 1], 0, 0).expect("add_element failed");
        let b = t.add_element_with_inspect("B", vec![0, 1], 0, 0).expect("add_element failed");
        let c = t.add_element_with_inspect("C", vec![0, 1], 0, 0).expect("add_element failed");

        // C depends on A and B
        t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: c.clone(), level: ONE },
                requires: ElementLevel { element_id: a.clone(), level: ONE },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");

        t.add_dependency(
            &Dependency {
                dependent: ElementLevel { element_id: c.clone(), level: ONE },
                requires: ElementLevel { element_id: b.clone(), level: ONE },
            },
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");

        // Remove A, invalidating C -> A. C now depends on Unsatisfiable.
        t.remove_element(a);

        // Remove B, invalidating C -> B. C already depends on Unsatisfiable.
        // This should not panic.
        t.remove_element(b);

        let c_deps: Vec<_> =
            t.direct_dependencies(&ElementLevel { element_id: c.clone(), level: ONE }).collect();

        assert_eq!(c_deps.len(), 1);
        assert_eq!(c_deps[0].requires.element_id, t.get_unsatisfiable_element_id());
    }

    // When an element is removed, every dependency naming it should drop out of both dependency
    // indexes, leaving no stale entries for the removed element on either side.
    #[fuchsia::test]
    fn test_dependency_indexes_updated_on_remove() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);

        let a = t.add_element_with_inspect("A", vec![0, 1], 0, 0).expect("add_element failed");
        let b = t.add_element_with_inspect("B", vec![0, 1], 0, 0).expect("add_element failed");
        let c = t.add_element_with_inspect("C", vec![0, 1], 0, 0).expect("add_element failed");

        // C depends on B, and B depends on A.
        let c_b = Dependency {
            dependent: ElementLevel { element_id: c.clone(), level: ONE },
            requires: ElementLevel { element_id: b.clone(), level: ONE },
        };
        let b_a = Dependency {
            dependent: ElementLevel { element_id: b.clone(), level: ONE },
            requires: ElementLevel { element_id: a.clone(), level: ONE },
        };
        t.add_dependency(
            &c_b,
            OnRequiredElementRemoval::RemoveWithRequiredElement,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");
        t.add_dependency(
            &b_a,
            OnRequiredElementRemoval::RemoveWithRequiredElement,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");

        assert_eq!(t.dependencies_for_required_element(b.clone()).collect::<Vec<_>>(), [&c_b]);
        assert_eq!(t.dependencies_for_dependent_element(b.clone()).collect::<Vec<_>>(), [&b_a]);

        // Removing B removes both the dependency on B and B's own dependency on A, leaving no
        // stale entries in either index.
        t.remove_element(b.clone());
        assert_eq!(t.dependencies_for_required_element(a).count(), 0);
        assert_eq!(t.dependencies_for_required_element(b.clone()).count(), 0);
        assert_eq!(t.dependencies_for_dependent_element(b).count(), 0);
        assert_eq!(t.dependencies_for_dependent_element(c).count(), 0);
        assert!(t.deps_by_required_element.is_empty());
        assert!(t.deps_by_dependent_element.is_empty());
        assert!(t.removable_dependencies.is_empty());
    }

    /// Collects dependencies into a deterministic order for comparison, as the indexes are
    /// unordered.
    fn sorted_deps<'a>(deps: impl Iterator<Item = &'a Dependency>) -> Vec<Dependency> {
        let mut deps: Vec<Dependency> = deps.cloned().collect();
        deps.sort();
        deps
    }

    // When dependencies are added and removed individually, both indexes should stay in step,
    // and an element's entry should disappear entirely once its last dependency is gone.
    #[fuchsia::test]
    fn test_dependency_indexes_updated_on_add_remove_dependency() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);

        let a = t.add_element_with_inspect("A", vec![0, 1, 2], 0, 0).expect("add_element failed");
        let b = t.add_element_with_inspect("B", vec![0, 1, 2], 0, 0).expect("add_element failed");
        let c = t.add_element_with_inspect("C", vec![0, 1, 2], 0, 0).expect("add_element failed");

        // A depends on C at two levels, and B depends on C at one level.
        let a1_c1 = Dependency {
            dependent: ElementLevel { element_id: a.clone(), level: ONE },
            requires: ElementLevel { element_id: c.clone(), level: ONE },
        };
        let a2_c2 = Dependency {
            dependent: ElementLevel { element_id: a.clone(), level: TWO },
            requires: ElementLevel { element_id: c.clone(), level: TWO },
        };
        let b1_c1 = Dependency {
            dependent: ElementLevel { element_id: b.clone(), level: ONE },
            requires: ElementLevel { element_id: c.clone(), level: ONE },
        };
        for dep in [&a1_c1, &a2_c2, &b1_c1] {
            t.add_dependency(
                dep,
                OnRequiredElementRemoval::MakeUnsatisfiable,
                &mut EagerInspectWriter,
            )
            .expect("add_dependency failed");
        }

        assert_eq!(
            sorted_deps(t.dependencies_for_required_element(c.clone())),
            sorted_deps([&a1_c1, &a2_c2, &b1_c1].into_iter())
        );
        assert_eq!(
            sorted_deps(t.dependencies_for_dependent_element(a.clone())),
            sorted_deps([&a1_c1, &a2_c2].into_iter())
        );
        assert_eq!(sorted_deps(t.dependencies_for_dependent_element(b.clone())), [b1_c1.clone()]);
        // A dependency is only indexed for the elements it references.
        assert_eq!(t.dependencies_for_required_element(a.clone()).count(), 0);
        assert_eq!(t.dependencies_for_dependent_element(c.clone()).count(), 0);

        // Adding the same dependency twice must not duplicate index entries.
        assert!(matches!(
            t.add_dependency(
                &a1_c1,
                OnRequiredElementRemoval::MakeUnsatisfiable,
                &mut EagerInspectWriter
            ),
            Err(ModifyDependencyError::AlreadyExists)
        ));
        assert_eq!(t.dependencies_for_required_element(c.clone()).count(), 3);

        t.remove_dependency(&a1_c1).expect("remove_dependency failed");
        assert_eq!(
            sorted_deps(t.dependencies_for_required_element(c.clone())),
            sorted_deps([&a2_c2, &b1_c1].into_iter())
        );
        assert_eq!(sorted_deps(t.dependencies_for_dependent_element(a.clone())), [a2_c2.clone()]);

        // Once an element has no remaining dependencies, it is removed from the index entirely.
        t.remove_dependency(&a2_c2).expect("remove_dependency failed");
        assert!(!t.deps_by_dependent_element.contains_key(&a));

        t.remove_dependency(&b1_c1).expect("remove_dependency failed");
        assert!(t.deps_by_required_element.is_empty());
        assert!(t.deps_by_dependent_element.is_empty());
    }

    // When removing an element rewrites a dependency to point at the unsatisfiable element, both
    // indexes should follow the rewrite: the replacement is indexed under the unsatisfiable
    // element, and nothing remains indexed under the removed one.
    #[fuchsia::test]
    fn test_dependency_indexes_updated_on_unsatisfiable_replacement() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut t = Topology::new(inspect.root(), 0);

        let a = t.add_element_with_inspect("A", vec![0, 1], 0, 0).expect("add_element failed");
        let b = t.add_element_with_inspect("B", vec![0, 1], 0, 0).expect("add_element failed");

        let a1_b1 = Dependency {
            dependent: ElementLevel { element_id: a.clone(), level: ONE },
            requires: ElementLevel { element_id: b.clone(), level: ONE },
        };
        t.add_dependency(
            &a1_b1,
            OnRequiredElementRemoval::MakeUnsatisfiable,
            &mut EagerInspectWriter,
        )
        .expect("add_dependency failed");

        // Removing B replaces A's dependency with one on the unsatisfiable element, which must be
        // reflected in both indexes.
        t.remove_element(b.clone());
        let unsatisfiable = t.get_unsatisfiable_element_id();
        let deps_from_a = sorted_deps(t.dependencies_for_dependent_element(a.clone()));
        assert_eq!(deps_from_a.len(), 1);
        assert_eq!(deps_from_a[0].requires.element_id, unsatisfiable);
        assert_eq!(
            sorted_deps(t.dependencies_for_required_element(unsatisfiable)),
            [deps_from_a[0].clone()]
        );
        assert_eq!(t.dependencies_for_required_element(b).count(), 0);
    }
}
