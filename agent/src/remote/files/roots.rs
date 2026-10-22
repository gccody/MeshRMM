//! Where browsing starts: the computer's drives or `/` and its volumes,
//! the account's home, and the folders of the user signed in to it.
use meshrmm_protocol::{FilePlace, FilePlaceKind, FileReply, FileRoots};

use super::{Account, paths, platform};
use crate::remote::config::ExecutionMode;

pub(super) fn roots(mode: ExecutionMode, account: &Account) -> FileReply {
    let signed_in = platform::signed_in_places(mode);
    let mut places = Vec::new();
    // root's home, or the user's own when it isn't listed below already.
    if let Some(home) = &account.home {
        let home = paths::text(home);
        if !signed_in.iter().any(|place| place.path == home) {
            places.push(FilePlace {
                path: home,
                kind: FilePlaceKind::Home,
                user: account.label.clone(),
            });
        }
    }
    places.extend(signed_in);
    FileReply::Roots(FileRoots {
        account: account.label.clone(),
        roots: platform::roots(),
        places,
    })
}
