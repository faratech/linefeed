//! Process-local conversation identity. IRC names are mutable metadata, never
//! identities for delayed UI work or native viewports.

use std::collections::{BTreeMap, HashMap};
use std::ops::Index;

use super::{CaseMapping, Channel};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ConversationId(pub(crate) u64);

struct Conversation {
    target: String,
    channel: Channel,
}

pub(crate) struct ConversationStore {
    entries: BTreeMap<ConversationId, Conversation>,
    names: HashMap<String, ConversationId>,
    canonical: HashMap<String, ConversationId>,
    mapping: CaseMapping,
    channel_types: String,
    next_id: u64,
    revision: u64,
}

impl Default for ConversationStore {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            names: HashMap::new(),
            canonical: HashMap::new(),
            mapping: CaseMapping::Rfc1459,
            channel_types: "#&+!".into(),
            next_id: 1,
            revision: 0,
        }
    }
}

impl ConversationStore {
    fn canonicalize(&self, target: &str) -> String {
        Self::canonicalize_with(self.mapping, &self.channel_types, target)
    }

    fn canonicalize_with(mapping: CaseMapping, channel_types: &str, target: &str) -> String {
        if mapping == CaseMapping::Utf8Rfc8265
            && let Some(first) = target
                .chars()
                .next()
                .filter(|first| channel_types.contains(*first))
        {
            return format!(
                "{first}{}",
                mapping.canonicalize(&target[first.len_utf8()..])
            );
        }
        mapping.canonicalize(target)
    }

    /// A provisional default may build an index but must not collapse exact
    /// restored names before the server advertises its own mapping.
    pub(crate) fn provisional_support(&mut self, support: &super::NetworkSupport) {
        self.mapping = support.case_mapping;
        self.channel_types.clone_from(&support.channel_types);
        self.reindex();
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(crate) fn ids(&self) -> impl Iterator<Item = ConversationId> + '_ {
        self.entries.keys().copied()
    }
    pub(crate) fn target(&self, id: ConversationId) -> Option<&str> {
        self.entries.get(&id).map(|entry| entry.target.as_str())
    }
    pub(crate) fn channel(&self, id: ConversationId) -> Option<&Channel> {
        self.entries.get(&id).map(|entry| &entry.channel)
    }
    pub(crate) fn channel_mut(&mut self, id: ConversationId) -> Option<&mut Channel> {
        self.entries.get_mut(&id).map(|entry| &mut entry.channel)
    }
    pub(crate) fn lookup(&self, target: &str) -> Option<ConversationId> {
        // Prefer exact spelling during snapshot bootstrap: CASEMAPPING was
        // deliberately not persisted and the provisional default may collide.
        self.names
            .get(target)
            .copied()
            .or_else(|| self.canonical.get(&self.canonicalize(target)).copied())
    }
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.values().map(|entry| &entry.target)
    }
    pub(crate) fn values(&self) -> impl Iterator<Item = &Channel> {
        self.entries.values().map(|entry| &entry.channel)
    }
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut Channel> {
        self.entries.values_mut().map(|entry| &mut entry.channel)
    }
    pub(crate) fn iter(&self) -> ConversationIter<'_> {
        ConversationIter(self.entries.values())
    }
    pub(crate) fn get(&self, target: &str) -> Option<&Channel> {
        self.names.get(target).and_then(|id| self.channel(*id))
    }
    pub(crate) fn get_mut(&mut self, target: &str) -> Option<&mut Channel> {
        let id = *self.names.get(target)?;
        self.channel_mut(id)
    }
    #[cfg(test)]
    pub(crate) fn contains_key(&self, target: &str) -> bool {
        self.names.contains_key(target)
    }

    /// Exact insertion is also the compatibility boundary for old snapshots.
    /// Callers use lookup first when processing a wire name.
    pub(crate) fn insert(&mut self, target: String, channel: Channel) -> Option<Channel> {
        if let Some(id) = self.names.get(&target) {
            return Some(std::mem::replace(
                &mut self.entries.get_mut(id).unwrap().channel,
                channel,
            ));
        }
        let id = ConversationId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("conversation IDs exhausted");
        let canonical = self.canonicalize(&target);
        self.canonical.entry(canonical).or_insert(id);
        self.names.insert(target.clone(), id);
        self.entries.insert(id, Conversation { target, channel });
        self.revision += 1;
        None
    }
    pub(crate) fn remove(&mut self, target: &str) -> Option<Channel> {
        let id = *self.names.get(target)?;
        self.remove_id(id)
    }
    pub(crate) fn remove_id(&mut self, id: ConversationId) -> Option<Channel> {
        let entry = self.entries.remove(&id)?;
        self.names.remove(&entry.target);
        self.reindex();
        self.revision += 1;
        Some(entry.channel)
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.names.clear();
        self.canonical.clear();
        // Never reset next_id, including when changing endpoints.
        self.revision += 1;
    }

    /// The source ID survives an explicit rename, including collisions. The
    /// caller merges the retired payload and redirects its UI references.
    pub(crate) fn rename(
        &mut self,
        id: ConversationId,
        target: &str,
    ) -> Option<(ConversationId, Channel)> {
        if !self.entries.contains_key(&id) {
            return None;
        }
        let collision = self.lookup(target).filter(|other| *other != id);
        let retired =
            collision.and_then(|other| self.remove_id(other).map(|channel| (other, channel)));
        let entry = self.entries.get_mut(&id).unwrap();
        self.names.remove(&entry.target);
        entry.target = target.to_owned();
        self.names.insert(target.to_owned(), id);
        self.reindex();
        self.revision += 1;
        retired
    }

    fn reindex(&mut self) {
        self.canonical.clear();
        for (id, entry) in &self.entries {
            self.canonical
                .entry(Self::canonicalize_with(
                    self.mapping,
                    &self.channel_types,
                    &entry.target,
                ))
                .or_insert(*id);
        }
    }

    /// Negotiate a mapping without losing any payload. Oldest identity wins
    /// deterministically; returned payloads are merged by Session.
    pub(crate) fn set_mapping(
        &mut self,
        mapping: CaseMapping,
    ) -> Vec<(ConversationId, ConversationId, Channel)> {
        self.mapping = mapping;
        self.reindex();
        let collisions: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let survivor = self.canonical[&self.canonicalize(&entry.target)];
                (survivor != *id).then_some((survivor, *id))
            })
            .collect();
        let mut merged = Vec::new();
        for (survivor, retired) in collisions {
            if let Some(channel) = self.remove_id(retired) {
                merged.push((survivor, retired, channel));
            }
        }
        self.revision += 1;
        merged
    }
}

pub(crate) struct ConversationIter<'a>(
    std::collections::btree_map::Values<'a, ConversationId, Conversation>,
);

impl<'a> Iterator for ConversationIter<'a> {
    type Item = (&'a String, &'a Channel);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|entry| (&entry.target, &entry.channel))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for ConversationIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0
            .next_back()
            .map(|entry| (&entry.target, &entry.channel))
    }
}

impl ExactSizeIterator for ConversationIter<'_> {}
impl std::iter::FusedIterator for ConversationIter<'_> {}

impl<'a> IntoIterator for &'a ConversationStore {
    type Item = (&'a String, &'a Channel);
    type IntoIter = ConversationIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Index<&str> for ConversationStore {
    type Output = Channel;
    fn index(&self, target: &str) -> &Self::Output {
        self.get(target).expect("conversation not found")
    }
}
impl Index<&String> for ConversationStore {
    type Output = Channel;
    fn index(&self, target: &String) -> &Self::Output {
        &self[target.as_str()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(store: &mut ConversationStore, target: &str, marker: &str) -> ConversationId {
        let mut channel = Channel::new();
        channel.key = Some(marker.to_string());
        assert!(store.insert(target.to_string(), channel).is_none());
        store.lookup(target).unwrap()
    }

    #[test]
    fn removed_and_cleared_conversations_never_reuse_an_id() {
        let mut store = ConversationStore::default();
        let first = insert(&mut store, "#room", "first");
        assert_eq!(store.remove("#room").unwrap().key.as_deref(), Some("first"));
        assert!(store.channel(first).is_none());
        assert!(store.target(first).is_none());
        let reopened = insert(&mut store, "#room", "reopened");
        assert!(reopened > first);
        let other = insert(&mut store, "bob", "other");

        store.clear();
        assert!(store.is_empty());
        for retired in [first, reopened, other] {
            assert!(store.target(retired).is_none());
            assert!(store.channel(retired).is_none());
        }
        assert!(store.lookup("#room").is_none());
        assert!(store.lookup("bob").is_none());
        let after_clear = insert(&mut store, "#room", "after clear");
        assert!(after_clear > other);
        assert_eq!(store.ids().collect::<Vec<_>>(), vec![after_clear]);
    }

    #[test]
    fn replacing_an_exact_entry_updates_the_payload_without_replacing_its_id() {
        let mut store = ConversationStore::default();
        let original = insert(&mut store, "#Room", "old payload");
        let mut replacement = Channel::new();
        replacement.key = Some("new payload".into());

        let previous = store.insert("#Room".into(), replacement).unwrap();

        assert_eq!(previous.key.as_deref(), Some("old payload"));
        assert_eq!(store.lookup("#Room"), Some(original));
        assert_eq!(
            store.channel(original).unwrap().key.as_deref(),
            Some("new payload")
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_case_only_rename_changes_display_spelling_and_keeps_identity() {
        let mut store = ConversationStore::default();
        let original = insert(&mut store, "#Room", "retained");
        let revision = store.revision();

        assert!(store.rename(original, "#ROOM").is_none());

        assert_eq!(store.target(original), Some("#ROOM"));
        assert_eq!(store.lookup("#room"), Some(original));
        assert_eq!(store.lookup("#Room"), Some(original));
        assert_eq!(
            store.channel(original).unwrap().key.as_deref(),
            Some("retained")
        );
        assert!(!store.contains_key("#Room"));
        assert!(store.contains_key("#ROOM"));
        assert!(store.revision() > revision);
    }

    #[test]
    fn explicit_rename_collision_keeps_the_source_even_when_destination_is_older() {
        let mut store = ConversationStore::default();
        let destination = insert(&mut store, "bob", "destination payload");
        let source = insert(&mut store, "alice", "source payload");
        assert!(destination < source);

        let (retired, payload) = store.rename(source, "BOB").unwrap();

        assert_eq!(retired, destination);
        assert_eq!(payload.key.as_deref(), Some("destination payload"));
        assert_eq!(store.target(source), Some("BOB"));
        assert_eq!(store.lookup("bob"), Some(source));
        assert_eq!(
            store.channel(source).unwrap().key.as_deref(),
            Some("source payload")
        );
        assert!(store.channel(destination).is_none());
        assert!(store.target(destination).is_none());
        assert!(store.lookup("alice").is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn negotiated_mapping_collisions_keep_oldest_ids_and_return_each_retired_payload() {
        let mut store = ConversationStore::default();
        assert!(store.set_mapping(CaseMapping::Ascii).is_empty());
        let brackets = insert(&mut store, "#Room[", "old brackets");
        let braces = insert(&mut store, "#Room{", "new braces");
        let slash = insert(&mut store, "#Room\\", "old slash");
        let pipe = insert(&mut store, "#Room|", "new pipe");
        let distinct = insert(&mut store, "#other", "distinct");

        let retired = store.set_mapping(CaseMapping::Rfc1459);

        assert_eq!(retired.len(), 2);
        assert_eq!((retired[0].0, retired[0].1), (brackets, braces));
        assert_eq!(retired[0].2.key.as_deref(), Some("new braces"));
        assert_eq!((retired[1].0, retired[1].1), (slash, pipe));
        assert_eq!(retired[1].2.key.as_deref(), Some("new pipe"));
        assert_eq!(store.lookup("#ROOM{"), Some(brackets));
        assert_eq!(store.lookup("#ROOM["), Some(brackets));
        assert_eq!(store.lookup("#ROOM|"), Some(slash));
        assert_eq!(store.lookup("#ROOM\\"), Some(slash));
        assert!(store.channel(braces).is_none());
        assert!(store.channel(pipe).is_none());
        assert_eq!(
            store.ids().collect::<Vec<_>>(),
            vec![brackets, slash, distinct]
        );
        assert!(store.set_mapping(CaseMapping::Rfc1459).is_empty());
    }

    #[test]
    fn exact_snapshot_entries_survive_until_the_server_negotiates_its_mapping() {
        let mut store = ConversationStore::default();
        // A snapshot can have come from an ASCII server. The provisional
        // RFC1459 default must not merge its exact target spellings on insert.
        let brackets = insert(&mut store, "#Room[", "brackets");
        let braces = insert(&mut store, "#Room{", "braces");
        assert_ne!(brackets, braces);
        assert_eq!(store.lookup("#Room["), Some(brackets));
        assert_eq!(store.lookup("#Room{"), Some(braces));
        assert_eq!(
            store.get("#Room[").unwrap().key.as_deref(),
            Some("brackets")
        );
        assert_eq!(store.get("#Room{").unwrap().key.as_deref(), Some("braces"));
        assert_eq!(store.len(), 2);

        assert!(store.set_mapping(CaseMapping::Ascii).is_empty());
        assert_eq!(store.lookup("#ROOM["), Some(brackets));
        assert_eq!(store.lookup("#ROOM{"), Some(braces));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn removing_the_oldest_bootstrap_alias_reindexes_to_the_remaining_entry() {
        let mut store = ConversationStore::default();
        let oldest = insert(&mut store, "#Room[", "oldest");
        let remaining = insert(&mut store, "#Room{", "remaining");
        assert_eq!(store.lookup("#ROOM["), Some(oldest));

        store.remove_id(oldest).unwrap();

        assert_eq!(store.lookup("#ROOM["), Some(remaining));
        assert_eq!(store.lookup("#ROOM{"), Some(remaining));
        assert_eq!(
            store.channel(remaining).unwrap().key.as_deref(),
            Some("remaining")
        );
    }

    #[test]
    fn borrowed_iteration_yields_the_original_entries_in_id_order() {
        let mut store = ConversationStore::default();
        let first = insert(&mut store, "#first", "first");
        let second = insert(&mut store, "#second", "second");
        let third = insert(&mut store, "#third", "third");
        let mut entries = (&store).into_iter();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries.size_hint(), (3, Some(3)));
        let (name, channel) = entries.next().unwrap();
        assert_eq!(name, "#first");
        assert!(std::ptr::eq(channel, store.channel(first).unwrap()));
        let (name, channel) = entries.next_back().unwrap();
        assert_eq!(name, "#third");
        assert!(std::ptr::eq(channel, store.channel(third).unwrap()));
        assert_eq!(entries.len(), 1);
        let (name, channel) = entries.next().unwrap();
        assert_eq!(name, "#second");
        assert!(std::ptr::eq(channel, store.channel(second).unwrap()));
        assert_eq!(entries.len(), 0);
        assert!(entries.next().is_none());
        assert!(entries.next_back().is_none());
        assert!(entries.next().is_none());
    }

    #[test]
    fn store_lookup_matches_negotiated_unicode_mapping_and_channel_prefixes() {
        let support = super::super::NetworkSupport {
            case_mapping: CaseMapping::Utf8Rfc8265,
            channel_types: "#&+".into(),
            ..super::super::NetworkSupport::default()
        };
        let mut store = ConversationStore::default();
        store.provisional_support(&support);
        for (original, alternate) in [
            ("#Éclair", "#éclair"),
            ("+Éclair", "+éclair"),
            ("&Éclair", "&éclair"),
            ("Éclair", "éclair"),
        ] {
            assert!(support.identifiers_equal(original, alternate));
            let id = insert(&mut store, original, original);
            assert_eq!(store.lookup(alternate), Some(id));
            assert_eq!(store.target(id), Some(original));
        }
        assert_eq!(store.len(), 4);
    }

    #[test]
    fn provisional_support_reindexes_without_merging_restored_conversations() {
        let mut store = ConversationStore::default();
        let older = insert(&mut store, "#Éclair", "older");
        let newer = insert(&mut store, "#éclair", "newer");
        let support = super::super::NetworkSupport {
            case_mapping: CaseMapping::Utf8Rfc8265,
            ..super::super::NetworkSupport::default()
        };
        store.provisional_support(&support);
        assert_eq!(store.lookup("#Éclair"), Some(older));
        assert_eq!(store.lookup("#éclair"), Some(newer));
        assert_eq!(store.len(), 2);

        let retired = store.set_mapping(support.case_mapping);
        assert_eq!(retired.len(), 1);
        assert_eq!((retired[0].0, retired[0].1), (older, newer));
        assert_eq!(retired[0].2.key.as_deref(), Some("newer"));
        assert_eq!(store.lookup("#éclair"), Some(older));
        assert_eq!(store.len(), 1);
    }
}
