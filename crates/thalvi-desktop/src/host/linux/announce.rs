//! Says Thalvi's status to a Linux screen reader over AT-SPI: one polite live region that never takes focus.

use std::sync::{Arc, Mutex, PoisonError};

use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Live, Node, NodeId, Role,
    TreeId, TreeInfo, TreeUpdate,
};
use accesskit_unix::Adapter;

use crate::host::StatusAnnouncer;

/// The tree's only window, named for the app; it never asks for focus.
const WINDOW: NodeId = NodeId(0);
/// The live region whose name a screen reader reads out.
const STATUS: NodeId = NodeId(1);

/// Starts the AT-SPI adapter that carries Thalvi's status to a screen reader.
pub fn status_announcer() -> Option<Arc<dyn StatusAnnouncer>> {
    Some(Arc::new(Announcer::atspi()))
}

/// Applies the tree `build` makes only while a screen reader has the tree.
trait ScreenReader: Send + 'static {
    fn update_if_active(&mut self, build: &mut dyn FnMut() -> TreeUpdate);
}

/// Carries each announcement into the live region, and answers the tree a screen reader asks for.
struct Announcer<S: ScreenReader> {
    reader: Mutex<(S, String)>,
}

impl Announcer<Atspi> {
    fn atspi() -> Self {
        let adapter = Adapter::new(LiveRegion, NoActions, NoDeactivation);
        Self {
            reader: Mutex::new((Atspi(adapter), String::new())),
        }
    }
}

#[cfg(test)]
impl<S: ScreenReader> Announcer<S> {
    fn with(reader: S) -> Self {
        Self {
            reader: Mutex::new((reader, String::new())),
        }
    }
}

impl<S: ScreenReader> StatusAnnouncer for Announcer<S> {
    fn announce(&self, announcement: &str) {
        let (reader, last) = &mut *self.reader.lock().unwrap_or_else(PoisonError::into_inner);
        if last == announcement {
            // AT-SPI needs a name change even when two captures produce the same announcement.
            reader.update_if_active(&mut || update("", Live::Off));
        }
        reader.update_if_active(&mut || {
            announcement.clone_into(last);
            update(announcement, Live::Polite)
        });
    }
}

/// The tree a screen reader is given when it connects: the window and its live region.
fn tree() -> TreeUpdate {
    let mut window = Node::new(Role::Window);
    window.set_label("Thalvi");
    window.set_children(vec![STATUS]);
    TreeUpdate {
        nodes: vec![(WINDOW, window), (STATUS, live_region("", Live::Polite))],
        tree: Some(TreeInfo::new(WINDOW)),
        tree_id: TreeId::ROOT,
        focus: WINDOW,
    }
}

fn live_region(name: &str, live: Live) -> Node {
    let mut region = Node::new(Role::Status);
    region.set_label(name);
    region.set_live(live);
    region
}

fn update(name: &str, live: Live) -> TreeUpdate {
    TreeUpdate {
        nodes: vec![(STATUS, live_region(name, live))],
        tree: None,
        tree_id: TreeId::ROOT,
        focus: WINDOW,
    }
}

struct LiveRegion;

impl ActivationHandler for LiveRegion {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        Some(tree())
    }
}

struct NoActions;

impl ActionHandler for NoActions {
    fn do_action(&mut self, _request: ActionRequest) {}
}

struct NoDeactivation;

impl DeactivationHandler for NoDeactivation {
    fn deactivate_accessibility(&mut self) {}
}

/// The AT-SPI adapter itself.
struct Atspi(Adapter);

impl ScreenReader for Atspi {
    fn update_if_active(&mut self, build: &mut dyn FnMut() -> TreeUpdate) {
        self.0.update_if_active(build);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use accesskit::{Live, Role};

    use super::*;

    /// A screen reader that is (or is not) attached to the application tree, as AccessKit knows.
    struct Reader {
        attached: bool,
        offered: Arc<Mutex<usize>>,
        updates: Arc<Mutex<Vec<TreeUpdate>>>,
    }

    type Offered = Arc<Mutex<usize>>;
    type Updates = Arc<Mutex<Vec<TreeUpdate>>>;

    impl ScreenReader for Reader {
        fn update_if_active(&mut self, build: &mut dyn FnMut() -> TreeUpdate) {
            *self.offered.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            if self.attached {
                self.updates
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(build());
            }
        }
    }

    fn reader(attached: bool) -> (Reader, Offered, Updates) {
        let offered = Arc::new(Mutex::new(0));
        let updates = Arc::new(Mutex::new(Vec::new()));
        (
            Reader {
                attached,
                offered: Arc::clone(&offered),
                updates: Arc::clone(&updates),
            },
            offered,
            updates,
        )
    }

    #[test]
    fn a_status_announcement_reaches_one_polite_live_region_named_for_it() {
        let (reader, offered, updates) = reader(true);
        let announcer = Announcer::with(reader);
        announcer.announce("Finishing");
        assert_eq!(
            *offered.lock().unwrap(),
            1,
            "one announcement is one update"
        );
        let recorded = updates.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let initial = LiveRegion.request_initial_tree().unwrap();
        assert_eq!(initial.tree_id, TreeId::ROOT);
        assert_eq!(initial.tree.as_ref().map(|tree| tree.root), Some(WINDOW));
        assert_eq!(initial.focus, WINDOW);
        let [(id, window), (child, region)] = initial.nodes.as_slice() else {
            panic!("one window and one live region");
        };
        assert_eq!(*id, WINDOW);
        assert_eq!(*child, STATUS);
        assert_eq!(region.role(), Role::Status);
        assert_eq!(region.live(), Some(Live::Polite));
        assert_eq!(region.label(), Some(""));
        assert_eq!(window.role(), Role::Window);
        assert_eq!(window.label(), Some("Thalvi"));
        assert_eq!(window.children(), [STATUS].as_slice());
        let [(id, region)] = recorded[0].nodes.as_slice() else {
            panic!("only the live region is updated");
        };
        assert_eq!(*id, STATUS);
        assert_eq!(region.role(), Role::Status);
        assert_eq!(region.live(), Some(Live::Polite));
        assert_eq!(region.label(), Some("Finishing"));
        drop(recorded);
        announcer.announce("Finishing");
        let updates = updates.lock().unwrap();
        let [_, clear, repeat] = updates.as_slice() else {
            panic!("a repeated announcement resets the live region first");
        };
        assert_eq!(clear.nodes[0].1.live(), Some(Live::Off));
        assert_eq!(repeat.nodes[0].1.live(), Some(Live::Polite));
        assert_eq!(repeat.nodes[0].1.label(), Some("Finishing"));
    }

    #[test]
    fn no_screen_reader_means_an_announcement_builds_no_tree() {
        let (reader, offered, updates) = reader(false);
        let announcer = Announcer::with(reader);
        announcer.announce("Finishing");
        announcer.announce("Sent to kitty");
        assert_eq!(*offered.lock().unwrap(), 2, "every announcement is offered");
        assert!(updates.lock().unwrap().is_empty(), "no tree is built");
    }
}
