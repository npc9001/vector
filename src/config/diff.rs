use std::collections::HashSet;

use indexmap::IndexMap;

use super::{ComponentKey, Config};

pub struct ConfigDiff {
    pub(crate) sources: Difference,
    pub(crate) transforms: Difference,
    pub(crate) sinks: Difference,
    pub(crate) enrichment_tables: Difference,
}

impl ConfigDiff {
    pub fn initial(initial: &Config) -> Self {
        Self::new(&Config::default(), initial)
    }

    pub fn new(old: &Config, new: &Config) -> Self {
        ConfigDiff {
            sources: Difference::new(&old.sources, &new.sources),
            transforms: Difference::new(&old.transforms, &new.transforms),
            sinks: Difference::new(&old.sinks, &new.sinks),
            enrichment_tables: Difference::new(&old.enrichment_tables, &new.enrichment_tables),
        }
    }

    /// Swaps removed with added in Differences.
    pub fn flip(mut self) -> Self {
        self.sources.flip();
        self.transforms.flip();
        self.sinks.flip();
        self
    }
}

pub struct Difference {
    pub to_remove: HashSet<ComponentKey>,
    pub(crate) to_change: HashSet<ComponentKey>,
    pub(crate) to_add: HashSet<ComponentKey>,
}

impl Difference {
    fn new<C>(old: &IndexMap<ComponentKey, C>, new: &IndexMap<ComponentKey, C>) -> Self
    where
        C: serde::Serialize + serde::Deserialize<'static>,
    {
        let old_names = old.keys().cloned().collect::<HashSet<_>>();
        let new_names = new.keys().cloned().collect::<HashSet<_>>();

        let to_change = old_names
            .intersection(&new_names)
            .filter(|&n| {
                // This is a hack around the issue of comparing two
                // trait objects. Json is used here over toml since
                // toml does not support serializing `None`.
                let old_json = serde_json::to_vec(&old[n]).unwrap();
                let new_json = serde_json::to_vec(&new[n]).unwrap();
                old_json != new_json
            })
            .cloned()
            .collect::<HashSet<_>>();

        let to_remove = &old_names - &new_names;
        let to_add = &new_names - &old_names;

        Self {
            to_remove,
            to_change,
            to_add,
        }
    }

    /// True if name is present in new config and either not in the old one or is different.
    pub fn contains_new(&self, id: &ComponentKey) -> bool {
        self.to_add.contains(id) || self.to_change.contains(id)
    }

    fn flip(&mut self) {
        std::mem::swap(&mut self.to_remove, &mut self.to_add);
    }

    pub fn changed_and_added(&self) -> impl Iterator<Item = &ComponentKey> {
        self.to_change.iter().chain(self.to_add.iter())
    }

    pub fn removed_and_changed(&self) -> impl Iterator<Item = &ComponentKey> {
        self.to_change.iter().chain(self.to_remove.iter())
    }
}
