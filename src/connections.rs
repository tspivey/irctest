#[cfg(test)]
mod tests {
    use super::*;
    fn update(id: usize) -> NetworkUpdate {
        NetworkUpdate::Upsert(crate::bouncer::Network {
            id: id.to_string(),
            attributes: Default::default(),
        })
    }
    #[test]
    fn retries_share_registration_slots_and_removed_networks_stay_stopped() {
        let mut g = Group::default();
        for id in 0..6 {
            g.apply(update(id));
        }
        g.registration_finished("0");
        assert!(g.registration_due("0").is_empty());
        assert_eq!(g.active_registrations(), 4);
        assert_eq!(
            g.registration_finished("1"),
            vec![ChildAction::Start("5".into())]
        );
        assert_eq!(
            g.registration_finished("2"),
            vec![ChildAction::Start("0".into())]
        );
        assert_eq!(g.active_registrations(), 4);
        g.apply(NetworkUpdate::Remove("0".into()));
        assert!(g.registration_due("0").is_empty());
    }
    #[test]
    fn manual_resume_clears_optout_and_parent_disconnect_cancels_queue() {
        let mut a = Group::default();
        let mut b = Group::default();
        assert_eq!(a.apply(update(42)), vec![ChildAction::Start("42".into())]);
        assert_eq!(b.apply(update(42)), vec![ChildAction::Start("42".into())]);
        a.suppress("42");
        assert!(a.apply(update(42)).is_empty());
        assert_eq!(a.resume("42"), vec![ChildAction::Start("42".into())]);
        a.disconnect();
        assert!(a.apply(update(43)).is_empty());
        assert_eq!(b.active_registrations(), 1);
    }
    #[test]
    fn bounded_fanout_optout_and_removal() {
        let mut g = Group::default();
        for id in 0..10 {
            g.apply(update(id));
        }
        assert_eq!(g.active_registrations(), 4);
        assert_eq!(
            g.registration_finished("0"),
            vec![ChildAction::Start("4".into())]
        );
        assert!(g.apply(update(0)).is_empty());
        assert_eq!(
            g.suppress("1"),
            vec![
                ChildAction::Stop("1".into()),
                ChildAction::Start("5".into())
            ]
        );
        assert!(g.apply(update(1)).is_empty());
        assert_eq!(
            g.apply(NetworkUpdate::Remove("6".into())),
            Vec::<ChildAction>::new()
        );
        assert_eq!(g.disconnect().len(), 5);
        assert_eq!(g.active_registrations(), 0);
    }
}

use crate::bouncer::{Network, NetworkUpdate};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildAction {
    Start(String),
    Stop(String),
}
#[derive(Debug, Default)]
pub struct Group {
    networks: BTreeMap<String, Network>,
    active: BTreeSet<String>,
    registering: BTreeSet<String>,
    suppressed: BTreeSet<String>,
    queue: VecDeque<String>,
    disconnected: bool,
}
impl Group {
    fn drain(&mut self) -> Vec<ChildAction> {
        let mut actions = Vec::new();
        while !self.disconnected && self.registering.len() < 4 {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            if !self.networks.contains_key(&id)
                || self.suppressed.contains(&id)
                || !self.active.insert(id.clone())
            {
                continue;
            }
            self.registering.insert(id.clone());
            actions.push(ChildAction::Start(id));
        }
        actions
    }
    pub fn apply(&mut self, update: NetworkUpdate) -> Vec<ChildAction> {
        let mut actions = vec![];
        match update {
            NetworkUpdate::Upsert(n) => {
                let id = n.id.clone();
                self.networks.insert(id.clone(), n);
                if !self.active.contains(&id) && !self.queue.contains(&id) {
                    self.queue.push_back(id);
                }
            }
            NetworkUpdate::Remove(id) => {
                self.networks.remove(&id);
                self.queue.retain(|n| n != &id);
                self.registering.remove(&id);
                self.suppressed.remove(&id);
                if self.active.remove(&id) {
                    actions.push(ChildAction::Stop(id));
                }
            }
        }
        actions.extend(self.drain());
        actions
    }
    /// Retries acquire the same slots as newly discovered networks.
    pub fn registration_due(&mut self, id: &str) -> Vec<ChildAction> {
        if self.disconnected || self.suppressed.contains(id) || !self.networks.contains_key(id) {
            return vec![];
        }
        self.active.remove(id);
        self.registering.remove(id);
        if !self.queue.iter().any(|n| n == id) {
            self.queue.push_back(id.into());
        }
        self.drain()
    }
    pub fn network(&self, id: &str) -> Option<&Network> {
        self.networks.get(id)
    }
    pub fn registration_finished(&mut self, id: &str) -> Vec<ChildAction> {
        self.registering.remove(id);
        self.drain()
    }
    pub fn suppress(&mut self, id: &str) -> Vec<ChildAction> {
        self.suppressed.insert(id.into());
        self.queue.retain(|n| n != id);
        self.registering.remove(id);
        let mut actions = vec![];
        if self.active.remove(id) {
            actions.push(ChildAction::Stop(id.into()));
        }
        actions.extend(self.drain());
        actions
    }
    pub fn resume(&mut self, id: &str) -> Vec<ChildAction> {
        self.disconnected = false;
        self.suppressed.remove(id);
        if self.networks.contains_key(id)
            && !self.active.contains(id)
            && !self.queue.iter().any(|n| n == id)
        {
            self.queue.push_back(id.into());
        }
        self.drain()
    }
    pub fn reconnect(&mut self) {
        self.disconnected = false;
        self.suppressed.clear();
    }
    pub fn disconnect(&mut self) -> Vec<ChildAction> {
        self.disconnected = true;
        self.queue.clear();
        self.registering.clear();
        std::mem::take(&mut self.active)
            .into_iter()
            .map(ChildAction::Stop)
            .collect()
    }
    pub fn active_registrations(&self) -> usize {
        self.registering.len()
    }
}
