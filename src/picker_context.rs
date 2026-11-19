//! Shared UI lifetime and destination state for selection picker pages.

use crate::download::DownloadManager;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

/// Navigation, queue ownership, and destination state shared by file pickers.
///
/// Scheduling is deliberately kept by each picker: torrent selection retains a
/// live `Cell`, while a gallery enumeration captures a timestamp snapshot.
pub(crate) struct PickerContext<'a> {
    pub nav: &'a adw::NavigationView,
    pub manager: Rc<DownloadManager>,
    pub dest_dir: Rc<RefCell<String>>,
    pub close_card: Rc<dyn Fn()>,
}
